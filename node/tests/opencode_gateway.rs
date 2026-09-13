//! The policy-aware gateway in front of one session's OpenCode server.
//!
//! Everything here is asserted from both ends: what the operator's client got
//! back, and what actually reached the harness. The fake server is the same
//! one the adapter tests drive (`support/fake_opencode.rs`), so a refusal that
//! never reaches it is visible as an empty request log rather than as the
//! gateway's own account of itself.

#[path = "support/mod.rs"]
mod support;
use support::fake_opencode::{serve, Fake, Recorded, Seen, PERMISSION, SESSION};
use support::state;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use tracon::{
    adapter::NativeApi,
    config::Config,
    http::{
        api::AppState,
        auth::{self, AuthState},
    },
    policy::{Rule, Verdict},
    session::Manager,
    store::Store,
    stream::Bus,
};

use support::fake::FakeAdapter;

/// The workspace every request is pinned to, as the runner sees it.
const WORKSPACE: &str = "/work";
/// What the fake server's Basic credential is built from.
const PASSWORD: &str = "the-node-minted-this";
const TRACON_SESSION: &str = "s-gateway";
/// Peers the guard reads: this machine, and anywhere else.
const LOCAL: &str = "127.0.0.1:5000";
const REMOTE: &str = "203.0.113.7:5000";

struct Rig {
    app: axum::Router,
    store: Arc<Store>,
    manager: Manager,
    seen: Arc<Mutex<Seen>>,
}

impl Rig {
    /// A node with one running session whose harness is the fake OpenCode
    /// server, and the operator router with its guard layered on exactly as
    /// `serve` builds it.
    async fn new() -> Self {
        state::isolate();
        let fake = Fake::new("1.18.30", usize::MAX);
        let seen = fake.seen.clone();
        *fake.password.lock().unwrap() = PASSWORD.to_string();
        let endpoint = serve(fake).await;

        let store = Arc::new(Store::open_in_memory().unwrap());
        store.ensure_peer_node("n1").unwrap();
        let cfg = Arc::new(Config::default());
        let tools = Arc::new(tracon::mcp::Tools {
            broker: Arc::new(Default::default()),
            cfg: cfg.clone(),
            policy: tracon::policy::Policy::shipped_shared(),
            http: reqwest::Client::new(),
            session: Default::default(),
        });
        let manager = Manager::new(
            store.clone(),
            Bus::new(),
            cfg.clone(),
            "n1".into(),
            tools.clone(),
            Default::default(),
            Arc::new(tracon::runner::local::LocalBackend),
        );
        let state = AppState {
            manager: manager.clone(),
            cfg,
            adapter: Arc::new(FakeAdapter {
                tx: Arc::new(tokio::sync::Mutex::new(None)),
                tokens: Arc::new(tokio::sync::Mutex::new(0)),
            }),
            node_id: "n1".into(),
            tools,
            mesh: None,
            auth: Arc::new(AuthState::load(&store, "127.0.0.1".into())),
            enroll: Default::default(),
        };
        let app = tracon::http::router(state.clone())
            .layer(axum::middleware::from_fn_with_state(state, auth::guard));

        let mut row = support::rows::session_row(TRACON_SESSION, "n1", "personal");
        row.harness_id = "opencode".into();
        row.harness_session_id = Some(SESSION.into());
        store.insert_session(&row).unwrap();
        manager
            .register_native_api_for_test(TRACON_SESSION, native_api(endpoint))
            .await;

        Self {
            app,
            store,
            manager,
            seen,
        }
    }

    /// One request through the gateway, as the operator's own client makes
    /// it: on this machine, with a loopback `Host`.
    async fn call(&self, method: &str, tail: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.call_from(method, tail, body, LOCAL, &[]).await
    }

    async fn call_from(
        &self,
        method: &str,
        tail: &str,
        body: Option<Value>,
        peer: &str,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Value) {
        let uri = format!("/api/opencode/{TRACON_SESSION}/{tail}");
        let (status, _, body) = self.raw(method, &uri, body, peer, headers).await;
        (status, body)
    }

    /// One request at any operator URI, with a peer address the guard reads.
    async fn raw(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        peer: &str,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Vec<String>, Value) {
        let mut b = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "127.0.0.1:7420");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        if body.is_some() {
            b = b.header("content-type", "application/json");
        }
        let mut req = b
            .body(match body {
                Some(v) => Body::from(v.to_string()),
                None => Body::empty(),
            })
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let cookies: Vec<String> = res
            .headers()
            .get_all(axum::http::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok().map(str::to_string))
            .collect();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes).to_string();
        (
            status,
            cookies,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    fn requests(&self) -> Vec<Recorded> {
        self.seen.lock().unwrap().requests.clone()
    }

    fn forget_requests(&self) {
        self.seen.lock().unwrap().requests.clear();
    }

    fn events(&self) -> Vec<(String, Value)> {
        self.store
            .events_after(TRACON_SESSION, 0, 500)
            .unwrap()
            .into_iter()
            .map(|e| (e.kind, e.payload))
            .collect()
    }

    fn events_of(&self, kind: &str) -> Vec<Value> {
        self.events()
            .into_iter()
            .filter(|(k, _)| k == kind)
            .map(|(_, p)| p)
            .collect()
    }
}

fn native_api(endpoint: SocketAddr) -> NativeApi {
    use base64::Engine;
    let credential =
        base64::engine::general_purpose::STANDARD.encode(format!("opencode:{PASSWORD}"));
    NativeApi {
        base: format!("http://{endpoint}"),
        authorization: format!("Basic {credential}"),
        directory: WORKSPACE.into(),
        session_id: SESSION.into(),
    }
}

/// A readable route reaches the harness with the credential attached on the
/// node, and nothing on the way back tells the client what it was. This is
/// finding 4 in one assertion: the browser can drive the harness without ever
/// holding its password.
#[tokio::test]
async fn a_readable_route_is_forwarded_with_the_credential_injected_server_side() {
    let rig = Rig::new().await;
    let (status, body) = rig
        .call_from(
            "GET",
            "global/health",
            None,
            LOCAL,
            &[("cookie", "tracon_session=not-a-real-one")],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["version"], "1.18.30");
    assert!(
        !body.to_string().contains(PASSWORD),
        "the password must never be in what the client reads"
    );

    let seen = rig.requests();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(
        seen[0]
            .authorization
            .as_deref()
            .is_some_and(|value| value.starts_with("Basic ")),
        "the gateway must inject the harness credential: {seen:?}"
    );
    // The operator's own cookie is the node's business, not the harness's.
    assert_eq!(seen[0].cookie, None, "{seen:?}");
    assert_eq!(seen[0].directory_header.as_deref(), Some(WORKSPACE));
    assert_eq!(rig.seen.lock().unwrap().unauthenticated, 0);
}

/// Every route the manifest's deny list names, plus the trees around them.
/// Each is refused with a 403 that names the method and the path, recorded on
/// the session's own log, and — the part that matters — never sent.
#[tokio::test]
async fn the_deny_list_is_refused_and_nothing_reaches_the_harness() {
    let rig = Rig::new().await;
    let denied: &[(&str, &str)] = &[
        ("PATCH", "config"),
        ("PATCH", "global/config"),
        ("PUT", "auth/anthropic"),
        ("DELETE", "auth/anthropic"),
        ("GET", "provider/auth"),
        ("POST", "provider/anthropic/oauth/authorize"),
        ("POST", "global/upgrade"),
        ("PATCH", &session_path("")),
        ("POST", "experimental/worktree"),
        ("POST", "tui/execute-command"),
        ("POST", "sync/replay"),
        ("POST", "mcp/local/connect"),
        ("POST", &session_path("/share")),
        ("POST", "global/dispose"),
        ("POST", "instance/dispose"),
        ("POST", "project/git/init"),
        ("GET", "event"),
        ("GET", "global/event"),
        ("GET", "api/event"),
        ("POST", "api/session"),
        // Not on the list by name: not on the table either, so it fails closed.
        ("GET", "made/up/route"),
    ];
    for (method, tail) in denied {
        let (status, body) = rig.call(method, tail, Some(json!({}))).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{method} /{tail} should be refused: {body}"
        );
        let message = body["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains(method) && message.contains(tail),
            "the refusal must name the method and the path: {message}"
        );
    }
    assert!(
        rig.requests().is_empty(),
        "a refused call must never reach the harness: {:?}",
        rig.requests()
    );
    let refusals = rig.events_of("gateway_refused");
    assert_eq!(refusals.len(), denied.len(), "{refusals:?}");
    assert!(refusals
        .iter()
        .all(|r| r["gateway"] == "opencode" && r["reason"].is_string()));
}

fn session_path(suffix: &str) -> String {
    format!("session/{SESSION}{suffix}")
}

/// One Basic credential is authority over every session on the server
/// (finding 5). The path is what keeps one session's UI out of another's, so a
/// session id that is not this mount's is refused before anything is sent.
#[tokio::test]
async fn a_foreign_session_id_is_refused() {
    let rig = Rig::new().await;
    let (status, body) = rig
        .call(
            "POST",
            "api/session/ses_someoneelses0000000/prompt",
            Some(json!({ "prompt": { "text": "hi" } })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("another session's id"),
        "{body}"
    );
    assert!(rig.requests().is_empty());
    assert_eq!(rig.events_of("gateway_refused").len(), 1);
}

/// `?directory=` is an implicit authorization to any path the server can
/// reach. The gateway rewrites it rather than observing it, in both the v1 and
/// the v2 spelling, and refuses a body that names one.
#[tokio::test]
async fn the_directory_is_replaced_with_the_sessions_workspace() {
    let rig = Rig::new().await;
    let (status, _) = rig
        .call(
            "GET",
            "file/status?directory=%2Fetc&location%5Bdirectory%5D=%2Fetc&path=README.md",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let seen = rig.requests();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(
        seen[0].query.contains("path=README.md"),
        "the caller's own parameters survive: {:?}",
        seen[0].query
    );
    assert!(
        !seen[0].query.contains("etc"),
        "the caller's directory must not survive: {:?}",
        seen[0].query
    );
    assert!(
        seen[0].query.contains("directory=/work"),
        "{:?}",
        seen[0].query
    );
    assert_eq!(seen[0].directory_header.as_deref(), Some(WORKSPACE));

    // And in a body, on a route that would otherwise be forwarded.
    rig.forget_requests();
    let (status, body) = rig
        .call(
            "POST",
            &format!("api/session/{SESSION}/compact"),
            Some(json!({ "location": { "directory": "/etc" } })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(rig.requests().is_empty());
}

/// An `always` reply persists a grant inside the harness that tracon never
/// decided on (finding 2). The gateway narrows it, records the attempt, and
/// forwards the narrowed answer — all three, on every spelling of the route.
#[tokio::test]
async fn an_always_reply_is_rewritten_to_once_and_recorded() {
    let rig = Rig::new().await;
    let (status, _) = rig
        .call(
            "POST",
            &format!("api/session/{SESSION}/permission/{PERMISSION}/reply"),
            Some(json!({ "reply": "always", "save": [{ "resource": "**" }] })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let replies = rig.seen.lock().unwrap().replies.clone();
    assert_eq!(replies.len(), 1, "{replies:?}");
    assert_eq!(replies[0]["id"], PERMISSION);
    assert_eq!(replies[0]["body"]["reply"], "once");
    assert_eq!(replies[0]["body"]["save"], json!([]));

    let broadening = rig.events_of("policy_denied");
    assert_eq!(broadening.len(), 1, "{broadening:?}");
    assert_eq!(broadening[0]["decision"], "always_rewritten_to_once");
    assert_eq!(broadening[0]["permission_id"], PERMISSION);

    let answers = rig.events_of("permission_answer");
    assert_eq!(answers.len(), 1, "{answers:?}");
    assert_eq!(answers[0]["option_id"], "once");
    assert_eq!(answers[0]["broadening_refused"], true);

    // A `once` needs no narrowing and records no broadening.
    let (status, _) = rig
        .call(
            "POST",
            &format!("api/session/{SESSION}/permission/{PERMISSION}/reply"),
            Some(json!({ "reply": "once" })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(rig.events_of("policy_denied").len(), 1);
    assert_eq!(rig.events_of("permission_answer").len(), 2);
}

/// A PTY is arbitrary command execution with no permission check of its own
/// (finding 7), so it is default-deny; the capability is what opens it, and
/// the WebSocket still waits for Gate D.
#[tokio::test]
async fn a_pty_needs_an_explicitly_granted_terminal_capability() {
    let rig = Rig::new().await;
    let (status, body) = rig
        .call("POST", "pty", Some(json!({ "command": "bash" })))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("terminal"),
        "the refusal must say what is missing: {body}"
    );
    assert!(rig.requests().is_empty());
    assert_eq!(rig.events_of("gateway_refused").len(), 1);

    // Granted, the same call is forwarded — and the ticket exchange the
    // WebSocket needs is still Gate D's.
    rig.manager.policy().write().rules.push(Rule {
        id: "terminal-for-this-test".into(),
        verdict: Verdict::Allow,
        reason: "the operator granted a terminal on this channel".into(),
        kinds: vec!["capability".into()],
        matches: vec!["terminal".into()],
        args: Default::default(),
        channels: vec![],
    });
    let (status, body) = rig
        .call("POST", "pty", Some(json!({ "command": "bash" })))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(rig.requests().len(), 1);
    assert_eq!(rig.events_of("policy_allowed").len(), 1);

    rig.forget_requests();
    let (status, _) = rig.call("GET", "pty/pty_1/connect", None).await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    assert!(rig.requests().is_empty());
}

/// The operator guard answers before the gateway does: this mount is an
/// operator route and is reached on the operator's own terms, not the
/// harness's. Every refusal here happens before a byte is forwarded.
#[tokio::test]
async fn an_unauthenticated_or_cross_origin_request_never_reaches_the_gateway() {
    let rig = Rig::new().await;
    // A node with no token issued answers loopback and nobody else.
    let (status, ..) = rig
        .call_from("GET", "global/health", None, REMOTE, &[])
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Issue one the way `tracon auth issue` does, over loopback.
    let (status, ..) = rig
        .raw(
            "POST",
            "/api/auth/token",
            Some(json!({ "token_hash": auth::hash("trc1.secret") })),
            LOCAL,
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // A stranger with no credential is refused.
    let (status, ..) = rig
        .call_from("GET", "global/health", None, REMOTE, &[])
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // With the token exchanged for a cookie, the same client gets through…
    let (status, cookies, _) = rig
        .raw(
            "POST",
            "/api/login",
            Some(json!({ "token": "trc1.secret" })),
            REMOTE,
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let cookie = cookies
        .iter()
        .find(|c| c.starts_with("tracon_session="))
        .map(|c| c.split(';').next().unwrap().to_string())
        .expect("a session cookie");
    let (status, ..) = rig
        .call_from("GET", "global/health", None, REMOTE, &[("cookie", &cookie)])
        .await;
    assert_eq!(status, StatusCode::OK);

    // …and the same cookie from another origin does not. A page on
    // evil.example holding a stolen cookie must not drive the harness.
    let (status, body) = rig
        .call_from(
            "GET",
            "global/health",
            None,
            REMOTE,
            &[("cookie", &cookie), ("origin", "https://evil.example")],
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    assert_eq!(
        rig.requests().len(),
        1,
        "only the authenticated same-origin call may reach the harness: {:?}",
        rig.requests()
    );
}

/// A prompt typed into the native UI is the same event as one typed into
/// tracon's: it runs through the session manager, so the turn epoch, the
/// watchdog, the budget and the ledger all apply, and the raw POST is never
/// forwarded.
#[tokio::test]
async fn a_prompt_through_the_gateway_runs_through_the_session_manager() {
    let live = Live::start().await;

    // The tracon way.
    let (status, _) = live
        .operator("/api/sessions/{id}/prompt", "from tracon")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(live.await_prompt("from tracon").await);

    // The native-UI way, through the gateway.
    let (status, body) = live
        .gateway(
            &format!("api/session/{SESSION}/prompt"),
            "from the native UI",
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert!(live.await_prompt("from the native UI").await);

    // Same event kind, same payload shape, from the same code path.
    let prompts: Vec<Value> = live
        .store
        .events_after(&live.session_id, 0, 500)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "user_prompt")
        .map(|e| e.payload)
        .collect();
    assert_eq!(prompts.len(), 2, "{prompts:?}");
    assert_eq!(prompts[0]["text"], "from tracon");
    assert_eq!(prompts[1]["text"], "from the native UI");

    // Nothing was forwarded: the manager owns the prompt, not the harness API.
    assert!(
        live.seen.lock().unwrap().prompts.is_empty(),
        "the raw prompt must not be forwarded"
    );
}

/// A real session on the fake harness, started the way the node starts one so
/// the supervisor, the ledger and the manager's prompt entry are all in play.
struct Live {
    app: axum::Router,
    store: Arc<Store>,
    session_id: String,
    seen: Arc<Mutex<Seen>>,
}

impl Live {
    async fn start() -> Self {
        state::isolate();
        let dir = state::scratch("opencode-gateway-live");
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "t"],
            vec!["config", "commit.gpgsign", "false"],
            vec!["commit", "-q", "--allow-empty", "-m", "init"],
        ] {
            let st = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(&args)
                .status()
                .unwrap();
            assert!(st.success(), "git {args:?}");
        }

        let fake = Fake::new("1.18.30", usize::MAX);
        let seen = fake.seen.clone();
        *fake.password.lock().unwrap() = PASSWORD.to_string();
        let endpoint = serve(fake).await;

        let store = Arc::new(Store::open_in_memory().unwrap());
        store
            .put_node(&support::rows::node_row("n1", "live"))
            .unwrap();
        let mut cfg = Config::default();
        cfg.session.worktree_root = dir.join("worktrees");
        let cfg = Arc::new(cfg);
        let tools = Arc::new(tracon::mcp::Tools {
            broker: Arc::new(Default::default()),
            cfg: cfg.clone(),
            policy: tracon::policy::Policy::shipped_shared(),
            http: reqwest::Client::new(),
            session: Default::default(),
        });
        let adapter = Arc::new(FakeAdapter {
            tx: Arc::new(tokio::sync::Mutex::new(None)),
            tokens: Arc::new(tokio::sync::Mutex::new(10)),
        });
        let manager = Manager::new(
            store.clone(),
            Bus::new(),
            cfg.clone(),
            "n1".into(),
            tools.clone(),
            Default::default(),
            Arc::new(tracon::runner::local::LocalBackend),
        );
        let _ = tools.session.set(tracon::mcp::SessionAccess {
            store: store.clone(),
            manager: manager.clone(),
        });
        let app = tracon::http::router(AppState {
            manager: manager.clone(),
            cfg,
            adapter: adapter.clone(),
            node_id: "n1".into(),
            tools,
            mesh: None,
            auth: Arc::new(AuthState::new("127.0.0.1".into(), None)),
            enroll: Default::default(),
        });

        let row = manager
            .create(
                tracon::session::NewSession {
                    channel: "personal".into(),
                    repo_path: repo.to_string_lossy().into_owned(),
                    branch: None,
                    work_item_id: None,
                    model: "m/a".into(),
                    budget_tokens: Some(100_000),
                    initial_prompt: None,
                    node_id: None,
                    phase: tracon::session::Phase::Execute,
                    review_id: None,
                    base_sha: None,
                    workspace_id: None,
                },
                adapter,
            )
            .await
            .expect("the session starts");
        let session_id = row.id.clone();
        for _ in 0..400 {
            if store
                .get_session(&session_id)
                .unwrap()
                .is_some_and(|s| s.state == "running")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // The fake harness has no native API of its own; the gateway is what
        // is under test, not the adapter, so it is registered directly.
        manager
            .register_native_api_for_test(&session_id, native_api(endpoint))
            .await;
        Self {
            app,
            store,
            session_id,
            seen,
        }
    }

    async fn operator(&self, template: &str, text: &str) -> (StatusCode, Value) {
        let uri = template.replace("{id}", &self.session_id);
        self.post(&uri, json!({ "text": text })).await
    }

    async fn gateway(&self, tail: &str, text: &str) -> (StatusCode, Value) {
        let uri = format!("/api/opencode/{}/{tail}", self.session_id);
        self.post(&uri, json!({ "prompt": { "text": text } })).await
    }

    async fn post(&self, uri: &str, body: Value) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("host", "127.0.0.1:7420")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(LOCAL.parse::<SocketAddr>().unwrap()));
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn await_prompt(&self, text: &str) -> bool {
        for _ in 0..400 {
            let seen = self
                .store
                .events_after(&self.session_id, 0, 500)
                .unwrap()
                .into_iter()
                .any(|e| e.kind == "user_prompt" && e.payload["text"] == text);
            if seen {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }
}
