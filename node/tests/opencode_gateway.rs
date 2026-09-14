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
    /// The node, so a test can reach the live channel the native UI streams
    /// from — the one thing on this mount the gateway serves rather than
    /// forwards (finding 20).
    manager: Manager,
    seen: Arc<Mutex<Seen>>,
    /// The harness itself, so a test can put it in a state — a permission it
    /// is still blocked on — rather than only observe what it was asked.
    fake: Fake,
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
        let endpoint = serve(fake.clone()).await;

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
            fake,
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

    /// Open a streaming route and hand back the live response. Unlike
    /// [`Rig::raw`] this never reads the body to its end, because an event
    /// stream does not have one.
    async fn open(&self, tail: &str, headers: &[(&str, &str)]) -> axum::response::Response {
        let mut b = Request::builder()
            .method("GET")
            .uri(format!("/api/opencode/{TRACON_SESSION}/{tail}"))
            .header("host", "127.0.0.1:7420")
            .header("accept", "text/event-stream");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let mut req = b.body(Body::empty()).unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(LOCAL.parse::<SocketAddr>().unwrap()));
        self.app.clone().oneshot(req).await.unwrap()
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
        ("GET", "api/event"),
        // `GET /global/event` is no longer here: the node answers it itself
        // rather than forwarding it (finding 20). That it still reaches the
        // harness never, and that the other two unscoped streams stay refused,
        // is asserted in
        // `the_other_global_streams_are_still_refused_and_nothing_is_proxied`.
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
/// (finding 7), so it is default-deny; an operator grant bound to this session
/// and its workspace is what opens it, and the upgrade still needs a ticket
/// the node minted. The terminal capability has tests of its own in
/// `opencode_pty.rs`; this is the gateway's share of it.
#[tokio::test]
async fn a_pty_needs_an_explicitly_granted_terminal_capability() {
    let rig = Rig::new().await;
    let (status, body) = rig
        .call("POST", "pty", Some(json!({ "command": "/bin/bash" })))
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

    // Granted, the same call is forwarded — rewritten, and recorded.
    rig.store
        .authority_grant_insert(&tracon::store::AuthorityGrantRow {
            id: uuid::Uuid::now_v7().to_string(),
            action: tracon::authority::TERMINAL.into(),
            verdict: "allow".into(),
            target: tracon::authority::terminal_target(TRACON_SESSION, WORKSPACE),
            channel: "personal".into(),
            session_id: Some(TRACON_SESSION.into()),
            revision: None,
            expires_ms: Some(tracon::store::now_ms() + 600_000),
            revoked_ms: None,
            reason: "the operator opened a terminal for this session".into(),
            created_ms: tracon::store::now_ms(),
        })
        .unwrap();
    let (status, body) = rig
        .call("POST", "pty", Some(json!({ "command": "/bin/bash" })))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(rig.events_of("policy_allowed").len(), 1);
    assert_eq!(rig.events_of("pty_opened").len(), 1);

    // The upgrade is the node's own route, and it opens nothing without a
    // ticket the node minted.
    rig.forget_requests();
    let (status, _) = rig.call("GET", "pty/pty_1/connect", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
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
                    parent_session: None,
                    continued_from: None,
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

// ---------------------------------------------------------------------------
// The synthesised live channel (finding 20)
// ---------------------------------------------------------------------------

/// One frame off an SSE body: the `id:` the client would resume from, and the
/// parsed `data:` payload.
type Frame = (Option<String>, Value);

/// Read frames until `want` of them have arrived or the deadline passes.
///
/// An event stream has no end, so nothing here may wait for one — which is why
/// this exists beside the rig's `to_bytes`. Two kinds of frame are skipped
/// because they are liveness rather than news, exactly as the app skips them:
/// comment keep-alives, which carry no `data:` at all, and `server.heartbeat`.
///
/// The deadline is deliberately under the ten-second heartbeat, so a test that
/// asks for more frames than exist settles quickly and always the same way.
async fn frames(body: Body, want: usize) -> Vec<Frame> {
    use futures_util::StreamExt;

    let mut stream = body.into_data_stream();
    let mut buffer = String::new();
    let mut out: Vec<Frame> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    while out.len() < want {
        let Ok(Some(Ok(chunk))) = tokio::time::timeout_at(deadline, stream.next()).await else {
            break;
        };
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(at) = buffer.find("\n\n") {
            let block: String = buffer.drain(..at + 2).collect();
            let mut id = None;
            let mut data = String::new();
            for line in block.lines() {
                if let Some(rest) = line.strip_prefix("data:") {
                    data.push_str(rest.trim_start());
                } else if let Some(rest) = line.strip_prefix("id:") {
                    id = Some(rest.trim().to_string());
                }
            }
            if data.is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(&data).expect("an SSE frame is JSON");
            if value["payload"]["type"] == "server.heartbeat" {
                continue;
            }
            out.push((id, value));
        }
    }
    out
}

/// The `permission.v2.asked` the fake server raises on its server-wide stream,
/// which is what the adapter's pump hands the channel.
fn asked(session: &str, id: &str) -> Value {
    json!({
        "id": "evt_perm",
        "type": "permission.v2.asked",
        "data": {
            "id": id,
            "sessionID": session,
            "action": "bash",
            "resources": ["just test"],
            "source": { "type": "tool", "messageID": "msg_1", "callID": "call_1" },
        },
    })
}

/// Finding 20, closed: the page's one live channel now carries the permission.
///
/// The native app streams from `GET /global/event` and from nothing else, so
/// before this a permission request reached the node, blocked the harness, and
/// never appeared on screen. Here the ask goes in the way the adapter's pump
/// puts it in and comes out on the app's own socket in the app's own shape —
/// with no part of it forwarded from upstream.
#[tokio::test]
async fn a_permission_reaches_the_native_ui_on_the_synthesised_stream() {
    let rig = Rig::new().await;
    let events = rig.manager.native_events(TRACON_SESSION);

    let res = rig.open("global/event", &[]).await;
    assert_eq!(res.status(), StatusCode::OK);
    // The app's client refuses any other content type outright.
    assert_eq!(
        res.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.starts_with("text/event-stream")),
        Some(true),
    );

    // Published after the client is attached, exactly as a live ask arrives.
    events.offer(SESSION, &asked(SESSION, PERMISSION));

    let frames = frames(res.into_body(), 2).await;
    assert_eq!(frames.len(), 2, "{frames:?}");

    // First, as upstream does: what marks the server connected in the app and
    // makes it refetch its snapshots. It carries no id, so it never moves the
    // client's resume point.
    let (id, connected) = &frames[0];
    assert_eq!(connected["payload"]["type"], "server.connected");
    assert_eq!(connected["directory"], WORKSPACE);
    assert_eq!(*id, None);

    // Then the ask, in the global envelope the app's legacy path reads:
    // `{directory, payload:{id, type, properties}}`.
    let (id, event) = &frames[1];
    assert_eq!(event["directory"], WORKSPACE);
    let payload = &event["payload"];
    assert_eq!(
        payload["type"], "permission.asked",
        "the app runs its v2 adapter only on the v2 transport; this envelope \
         must already be v1 or the page switches on a name it does not know"
    );
    let properties = &payload["properties"];
    assert_eq!(properties["id"], PERMISSION);
    assert_eq!(properties["sessionID"], SESSION);
    assert_eq!(properties["permission"], "bash");
    assert_eq!(properties["patterns"], json!(["just test"]));
    assert_eq!(properties["always"], json!([]));
    assert_eq!(properties["tool"]["callID"], "call_1");
    // tracon's own sequence, which upstream has none of: this is what the v1
    // client sends back as `Last-Event-ID`.
    assert_eq!(id.as_deref(), Some("1"));

    // And nothing was forwarded. The harness was never asked for an event
    // stream: the route is served, not proxied.
    assert!(
        !rig.requests().iter().any(|r| r.path.contains("event")),
        "the synthesised stream reached the harness: {:?}",
        rig.requests()
    );
}

/// One OpenCode server holds more than one session, and one Basic credential
/// is authority over all of them. The channel is the browser's view of exactly
/// one, so a sibling session's ask is not on it.
#[tokio::test]
async fn another_sessions_events_are_never_forwarded_to_this_page() {
    let rig = Rig::new().await;
    let events = rig.manager.native_events(TRACON_SESSION);

    let res = rig.open("global/event", &[]).await;
    events.offer(SESSION, &asked("ses_other", "per_theirs"));
    events.offer(SESSION, &asked(SESSION, PERMISSION));

    // Asking for three and getting two is the assertion: the sibling's ask was
    // dropped before the broadcast, so only `server.connected` and this
    // session's own ask exist to be read.
    let frames = frames(res.into_body(), 3).await;
    assert_eq!(frames.len(), 2, "{frames:?}");
    assert_eq!(frames[0].1["payload"]["type"], "server.connected");
    assert_eq!(frames[1].1["payload"]["properties"]["id"], PERMISSION);
    assert!(
        !frames
            .iter()
            .any(|(_, f)| f.to_string().contains("per_theirs")),
        "another session's permission reached the page: {frames:?}"
    );
}

/// An ask raised while the page was away is still delivered, because the node
/// numbers what upstream never did. This is the half of finding 6 that made
/// the upstream stream unforwardable, fixed rather than wished away.
#[tokio::test]
async fn a_reconnecting_page_resumes_from_its_last_event_id() {
    let rig = Rig::new().await;
    let events = rig.manager.native_events(TRACON_SESSION);

    // Raised with nobody connected.
    events.offer(SESSION, &asked(SESSION, PERMISSION));

    // The v1 SSE client the app uses parses `id:` and resends it.
    let res = rig.open("global/event", &[("last-event-id", "0")]).await;
    let resumed = frames(res.into_body(), 2).await;
    assert_eq!(resumed.len(), 2, "{resumed:?}");
    assert_eq!(resumed[0].1["payload"]["type"], "server.connected");
    assert_eq!(resumed[1].1["payload"]["properties"]["id"], PERMISSION);
    assert_eq!(resumed[1].0.as_deref(), Some("1"));

    // A client that has already seen it does not get it twice.
    let res = rig.open("global/event", &[("last-event-id", "1")]).await;
    let again = frames(res.into_body(), 2).await;
    assert_eq!(
        again.len(),
        1,
        "a frame the client already had was re-delivered: {again:?}"
    );
    assert_eq!(again[0].1["payload"]["type"], "server.connected");
}

/// Serving `/global/event` is not the same as allowing it. The two remaining
/// unscoped, unreplayable streams stay refused by name, and the synthesised
/// route is one exact (method, path) rather than a hole in the tree.
#[tokio::test]
async fn the_other_global_streams_are_still_refused_and_nothing_is_proxied() {
    let rig = Rig::new().await;
    for tail in ["event", "api/event"] {
        let (status, body) = rig.call("GET", tail, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "GET /{tail}: {body}");
    }
    let (status, _) = rig.call("POST", "global/event", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = rig.call("GET", "global/event/all", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    assert!(
        rig.requests().is_empty(),
        "a refused stream still reached the harness: {:?}",
        rig.requests()
    );
    assert_eq!(rig.events_of("gateway_refused").len(), 4);
}

/// A permission raised before the page opened is on the page, and a sibling
/// session's is not.
///
/// This is how the app finds pending asks at all — it never learns them from
/// the stream (`bootstrap.ts`), so without it a request raised before the tab
/// opened stays invisible however good the stream is. The route is
/// instance-wide upstream: the path names no session, and pinning
/// `?directory=` does not separate siblings that share a workspace. So the
/// gateway scopes the body.
#[tokio::test]
async fn the_pending_permission_snapshot_is_scoped_to_this_session() {
    let rig = Rig::new().await;
    rig.fake
        .pending_permission_for(SESSION, PERMISSION, "call_1");
    rig.fake
        .pending_permission_for("ses_other", "per_theirs", "call_9");

    let (status, body) = rig.call("GET", "permission", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let listed = body.as_array().expect("a list of pending permissions");
    assert_eq!(listed.len(), 1, "{body}");
    assert_eq!(listed[0]["id"], PERMISSION);
    assert_eq!(listed[0]["sessionID"], SESSION);

    // The harness was asked; only the answer was narrowed. This is still a
    // readable route, not a thing the node makes up.
    assert!(
        rig.requests().iter().any(|r| r.path == "/permission"),
        "{:?}",
        rig.requests()
    );
}
