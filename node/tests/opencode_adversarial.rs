//! The adversarial run against the mediated OpenCode API (ROADMAP Gate B).
//!
//! The gateway tests next door prove the happy shapes: a readable route is
//! forwarded, the deny list is refused, an `always` becomes a `once`. These
//! are the ones that ask what happens when the caller is *trying* — a native
//! UI, a page that got through the operator guard, or a harness driving its
//! own server — to widen what it is allowed to do.
//!
//! Seven questions, each asked from both ends: what the client got back, and
//! what reached the harness. A refusal that never reaches the fake server is
//! visible here as an empty request log rather than as the gateway's own
//! account of itself.
//!
//! 1. permission escalation — `always`, a rewritten ruleset, saved grants, and
//!    a reply aimed at another session's request;
//! 2. foreign session ids and directory escapes;
//! 3. config and auth writes;
//! 4. share, revert, shell, and child sessions;
//! 5. a mutation whose answer never comes back;
//! 6. two live sessions that cannot read or migrate each other's state;
//! 7. the single-writer fence the harness does not have
//!    (`config-state.md` §7.2 row 6b).
//!
//! Where the property is about *OpenCode's* behaviour the pinned binary itself
//! is driven (skipped, with a message, when it is not on the machine); where it
//! is about tracon's gateway the fake server is, because what matters there is
//! the decision and not the harness.

#[path = "support/mod.rs"]
mod support;
use support::fake::FakeAdapter;
use support::fake_opencode::{serve, spec, Fake, HttpApi, Recorded, Seen, PERMISSION, SESSION};
use support::state;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use tracon::{
    adapter::{opencode::OpenCodeAdapter, HarnessAdapter, HarnessHandle, LaunchSpec, NativeApi},
    config::Config,
    gateway::native_events::NativeEvents,
    http::{
        api::AppState,
        auth::{self, AuthState},
    },
    policy::{Rule, Verdict},
    runner::{Runner, RunnerCommand, RunnerError, Spawned},
    session::{
        ingest::{Ingest, Reconcile},
        Manager,
    },
    store::{intent_state, object_kind, Store},
    stream::Bus,
};

/// The workspace every request is pinned to, as the runner sees it.
const WORKSPACE: &str = "/work";
const PASSWORD: &str = "the-node-minted-this";
const TRACON_SESSION: &str = "s-adversarial";
/// A second session on the same node, which this one must not be able to act
/// for.
const OTHER_SESSION: &str = "s-someone-elses";
const OTHER_UPSTREAM: &str = "ses_someoneelses000000000";
const OTHER_PERMISSION: &str = "per_someoneelses000000000";
const LOCAL: &str = "127.0.0.1:5000";

// ---------------------------------------------------------------------------
// The rig
// ---------------------------------------------------------------------------

struct Rig {
    app: axum::Router,
    store: Arc<Store>,
    manager: Manager,
    fake: Fake,
    seen: Arc<Mutex<Seen>>,
    endpoint: SocketAddr,
}

impl Rig {
    /// A node with one running session whose harness is the fake OpenCode
    /// server, plus a second session whose upstream ids the first must not be
    /// able to name.
    async fn new() -> Self {
        state::isolate();
        let fake = Fake::new("1.18.30", usize::MAX);
        let seen = fake.seen.clone();
        *fake.password.lock().unwrap() = PASSWORD.to_string();
        let endpoint = serve(fake.clone()).await;

        let store = Arc::new(Store::open_in_memory().unwrap());
        store.ensure_peer_node("n1").unwrap();
        let mut cfg = Config::default();
        // A forwarded call that does not report inside this is uncertain. One
        // second rather than the shipped thirty, so the test asks the question
        // in about a second.
        cfg.session.harness_api_timeout_secs = 1;
        let cfg = Arc::new(cfg);
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

        for (id, upstream) in [(TRACON_SESSION, SESSION), (OTHER_SESSION, OTHER_UPSTREAM)] {
            let mut row = support::rows::session_row(id, "n1", "personal");
            row.harness_id = "opencode".into();
            row.harness_session_id = Some(upstream.into());
            store.insert_session(&row).unwrap();
            store.opencode_bind(id, upstream, None).unwrap();
        }
        // The other session's permission request, mapped as ingestion maps one
        // when it is raised. This is how the node knows whose it is.
        store
            .opencode_map(
                OTHER_SESSION,
                object_kind::PERMISSION,
                OTHER_PERMISSION,
                Some("p-other"),
                None,
                1,
            )
            .unwrap();
        store
            .opencode_map(
                TRACON_SESSION,
                object_kind::PERMISSION,
                PERMISSION,
                Some("p-mine"),
                None,
                1,
            )
            .unwrap();
        manager
            .register_native_api_for_test(TRACON_SESSION, native_api(endpoint))
            .await;

        Self {
            app,
            store,
            manager,
            fake,
            seen,
            endpoint,
        }
    }

    async fn call(&self, method: &str, tail: &str, body: Option<Value>) -> (StatusCode, Value) {
        let uri = format!("/api/opencode/{TRACON_SESSION}/{tail}");
        self.raw(method, &uri, body, &[]).await
    }

    async fn raw(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Value) {
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
            .insert(ConnectInfo(LOCAL.parse::<SocketAddr>().unwrap()));
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes).to_string();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    fn requests(&self) -> Vec<Recorded> {
        self.seen.lock().unwrap().requests.clone()
    }

    fn forget_requests(&self) {
        self.seen.lock().unwrap().requests.clear();
    }

    fn events_of(&self, kind: &str) -> Vec<Value> {
        self.store
            .events_after(TRACON_SESSION, 0, 500)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == kind)
            .map(|e| e.payload)
            .collect()
    }

    /// Every refusal asserts the same three things: the status, that the
    /// harness never saw it, and that the operator can.
    async fn refused(&self, method: &str, tail: &str, body: Option<Value>) -> String {
        self.forget_requests();
        let before = self.events_of("gateway_refused").len();
        let (status, answer) = self.call(method, tail, body).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{method} /{tail} should be refused: {answer}"
        );
        assert!(
            self.requests().is_empty(),
            "{method} /{tail} reached the harness: {:?}",
            self.requests()
        );
        let refusals = self.events_of("gateway_refused");
        assert_eq!(
            refusals.len(),
            before + 1,
            "{method} /{tail} left no record for the operator"
        );
        answer["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .to_string()
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

fn allow(rig: &Rig, kind: &str, title: &str) {
    rig.manager.policy().write().rules.push(Rule {
        id: format!("allow-{title}-for-this-test"),
        verdict: Verdict::Allow,
        reason: format!("the operator granted {title} on this channel"),
        kinds: vec![kind.into()],
        matches: vec![title.into()],
        args: Default::default(),
        channels: vec![],
    });
}

/// The grant an operator makes to open a terminal: bound to this channel,
/// this session, and this session's workspace path, with an expiry. A policy
/// rule could allow the capability too, but a grant is what binds it, and the
/// binding is what the terminal tests are about.
fn grant_terminal(rig: &Rig) {
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
}

fn deny(rig: &Rig, kind: &str, title: &str) {
    rig.manager.policy().write().rules.push(Rule {
        id: format!("deny-{title}-for-this-test"),
        verdict: Verdict::Deny,
        reason: format!("{title} is denied on this channel"),
        kinds: vec![kind.into()],
        matches: vec![title.into()],
        args: Default::default(),
        channels: vec![],
    });
}

// ---------------------------------------------------------------------------
// 1. Permission escalation
// ---------------------------------------------------------------------------

/// Every route through which a caller could widen what the harness may do
/// without tracon deciding it: the `always` that persists a grant (finding 2),
/// the `PATCH` that rewrites the ruleset outright, and the saved-grant table
/// v2 keeps in its own database.
#[tokio::test]
async fn a_permission_grant_cannot_be_broadened_through_any_route() {
    let rig = Rig::new().await;

    // The reply that would persist a grant: narrowed on the wire, and both the
    // decision and the attempt are on the record.
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
    assert_eq!(replies[0]["body"]["reply"], "once");
    assert_eq!(
        replies[0]["body"]["save"],
        json!([]),
        "an empty save is the difference between a decision and a standing grant"
    );
    let broadening = rig.events_of("policy_denied");
    assert_eq!(broadening.len(), 1, "{broadening:?}");
    assert_eq!(broadening[0]["decision"], "always_rewritten_to_once");

    // The ruleset itself: `PATCH /session/{id}` can rewrite every permission
    // to `allow`, which would end the ask-everything policy the whole design
    // rests on (finding 1).
    let message = rig
        .refused(
            "PATCH",
            &format!("session/{SESSION}"),
            Some(json!({ "permission": { "*": "allow" } })),
        )
        .await;
    assert!(message.contains("ruleset"), "{message}");

    // The saved grants v2 persists: readable, never writable.
    let (status, _) = rig.call("GET", "api/permission/saved", None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the operator may read what is saved"
    );
    assert_eq!(rig.requests().len(), 1);
    rig.refused("DELETE", "api/permission/saved/sav_1", None)
        .await;
    rig.refused(
        "POST",
        "api/permission/saved",
        Some(json!({ "action": "bash", "resources": ["**"] })),
    )
    .await;
}

/// One Basic credential is authority over every session on the server
/// (finding 5). The v1 reply route names no session at all, so the only thing
/// that can tell whose request is being answered is the node's own identity
/// map — and it does.
#[tokio::test]
async fn a_reply_to_another_sessions_permission_is_refused() {
    let rig = Rig::new().await;

    // Named on a session-scoped route: refused on the path.
    let message = rig
        .refused(
            "POST",
            &format!("api/session/{OTHER_UPSTREAM}/permission/{OTHER_PERMISSION}/reply"),
            Some(json!({ "reply": "once" })),
        )
        .await;
    assert!(message.contains("another session's id"), "{message}");

    // Named on the route that carries no session: refused on the map.
    let message = rig
        .refused(
            "POST",
            &format!("permission/{OTHER_PERMISSION}/reply"),
            Some(json!({ "reply": "once" })),
        )
        .await;
    assert!(
        message.contains(OTHER_SESSION) && message.contains(OTHER_PERMISSION),
        "the refusal must name whose request it is: {message}"
    );

    // This session's own request on the same route is answered.
    let (status, _) = rig
        .call(
            "POST",
            &format!("permission/{PERMISSION}/reply"),
            Some(json!({ "reply": "once" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let seen = rig.requests();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(seen[0].path.ends_with(&format!("{PERMISSION}/reply")));
}

// ---------------------------------------------------------------------------
// 2. Foreign session ids and directory escapes
// ---------------------------------------------------------------------------

/// Every route whose pattern names a session refuses an id that is not this
/// mount's — readable ones included, because a transcript is as much the
/// other session's as its prompt route is.
#[tokio::test]
async fn every_route_that_names_a_session_refuses_a_foreign_id() {
    let rig = Rig::new().await;
    let foreign: &[(&str, String, Option<Value>)] = &[
        ("GET", format!("session/{OTHER_UPSTREAM}"), None),
        ("GET", format!("session/{OTHER_UPSTREAM}/message"), None),
        ("GET", format!("session/{OTHER_UPSTREAM}/diff"), None),
        ("GET", format!("api/session/{OTHER_UPSTREAM}"), None),
        ("GET", format!("api/session/{OTHER_UPSTREAM}/history"), None),
        ("GET", format!("api/session/{OTHER_UPSTREAM}/event"), None),
        (
            "GET",
            format!("api/session/{OTHER_UPSTREAM}/permission"),
            None,
        ),
        (
            "POST",
            format!("api/session/{OTHER_UPSTREAM}/prompt"),
            Some(json!({ "prompt": { "text": "act for them" } })),
        ),
        (
            "POST",
            format!("api/session/{OTHER_UPSTREAM}/interrupt"),
            Some(json!({})),
        ),
        (
            "POST",
            format!("session/{OTHER_UPSTREAM}/revert"),
            Some(json!({})),
        ),
        (
            "POST",
            format!("api/session/{OTHER_UPSTREAM}/compact"),
            Some(json!({})),
        ),
        (
            "POST",
            format!("api/session/{OTHER_UPSTREAM}/model"),
            Some(json!({ "model": "anthropic/claude-x" })),
        ),
    ];
    for (method, tail, body) in foreign {
        let message = rig.refused(method, tail, body.clone()).await;
        assert!(
            message.contains("another session's id"),
            "{method} /{tail}: {message}"
        );
    }
}

/// A path that does not normalise to one unambiguous route is refused before
/// anything is matched, let alone forwarded: `..`, an encoded separator, and a
/// second layer of encoding are all attempts to be classified as one route and
/// delivered as another.
#[tokio::test]
async fn a_path_that_does_not_normalise_is_refused_before_it_is_classified() {
    let rig = Rig::new().await;
    for tail in [
        "../config",
        "api/session/../../config",
        "api/%2e%2e/config",
        "api/fs/read/%2Fetc%2Fpasswd",
        "api%2Fsession",
        "api/%252e%252e/config",
        "api/session/ses_x%00/event",
    ] {
        let message = rig.refused("GET", tail, None).await;
        assert!(
            message.contains("normalise"),
            "/{tail} was refused for the wrong reason: {message}"
        );
    }
}

/// `?directory=` and its spellings are an implicit authorization to any path
/// the server process can reach (finding 5). The gateway pins its own on every
/// request rather than reading the caller's, and a body that names another one
/// is refused rather than rewritten — including `cwd`, which is what holds a
/// PTY to the workspace.
#[tokio::test]
async fn every_spelling_of_a_directory_is_replaced_or_refused() {
    let rig = Rig::new().await;

    // Query and header, on a readable route: replaced, not observed.
    let (status, _) = rig
        .raw(
            "GET",
            &format!(
                "/api/opencode/{TRACON_SESSION}/file/status\
                 ?directory=%2Fetc&workspace=%2Fetc&location%5Bdirectory%5D=%2Fetc&path=README.md"
            ),
            None,
            &[
                ("x-opencode-directory", "/etc"),
                ("x-opencode-workspace", "/etc"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let seen = rig.requests();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(!seen[0].query.contains("etc"), "{:?}", seen[0].query);
    assert!(seen[0].query.contains("directory=/work"), "{:?}", seen[0]);
    assert!(
        seen[0].query.contains("path=README.md"),
        "the caller's own parameters survive: {:?}",
        seen[0].query
    );
    assert_eq!(
        seen[0].directory_header.as_deref(),
        Some(WORKSPACE),
        "the caller's header must not survive"
    );

    // Bodies, on every mediated route that takes one. A directory is an
    // authorization, so it is refused rather than quietly corrected — and for
    // a PTY that holds even with the capability granted, because `cwd` is the
    // one field a granted terminal could otherwise use to leave the workspace.
    grant_terminal(&rig);
    for (method, tail, body) in [
        (
            "POST",
            format!("api/session/{SESSION}/compact"),
            json!({ "location": { "directory": "/etc" } }),
        ),
        (
            "POST",
            format!("session/{SESSION}/revert"),
            json!({ "directory": "/etc" }),
        ),
        (
            "POST",
            "pty".to_string(),
            json!({ "command": "bash", "cwd": "/" }),
        ),
        (
            "POST",
            "pty".to_string(),
            json!({ "command": "bash", "env": { "x": "y" }, "shell": { "worktree": "/etc" } }),
        ),
    ] {
        let message = rig.refused(method, &tail, Some(body)).await;
        assert!(
            message.contains("outside this session's workspace"),
            "{method} /{tail}: {message}"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. Config and auth writes
// ---------------------------------------------------------------------------

/// The node holds the credentials. Every route through which the harness's own
/// API would write one — or install the plugin that would read one — is
/// refused with nothing reaching the harness at all.
#[tokio::test]
async fn config_and_credential_writes_never_reach_the_harness() {
    let rig = Rig::new().await;
    let denied: &[(&str, &str)] = &[
        ("PATCH", "config"),
        ("PATCH", "global/config"),
        ("PUT", "auth/anthropic"),
        ("DELETE", "auth/anthropic"),
        ("GET", "auth/anthropic"),
        ("POST", "provider/anthropic/oauth/authorize"),
        ("POST", "provider/anthropic/oauth/exchange"),
        ("GET", "provider/auth"),
        ("PATCH", "api/credential/c1"),
        ("PUT", "api/credential/c1"),
        ("DELETE", "api/credential/c1"),
        ("GET", "api/credential"),
        ("POST", "api/integration/i1/connect/key"),
        ("GET", "api/integration"),
        ("POST", "global/upgrade"),
        ("POST", "mcp/local/connect"),
    ];
    for (method, tail) in denied {
        rig.refused(method, tail, Some(json!({ "key": "sk-planted" })))
            .await;
    }
    assert!(
        rig.seen
            .lock()
            .unwrap()
            .requests
            .iter()
            .all(|r| !r.path.contains("auth")),
        "nothing about credentials may reach the harness"
    );
}

// ---------------------------------------------------------------------------
// 4. Share, revert, shell, children
// ---------------------------------------------------------------------------

/// Sharing publishes the transcript off the node; a child session is a worker
/// nothing supervises; `init` writes instructions from inside the harness.
/// All three are visible refusals rather than silent omissions.
#[tokio::test]
async fn share_fork_init_and_agent_are_refused() {
    let rig = Rig::new().await;
    let message = rig
        .refused("POST", &format!("session/{SESSION}/share"), Some(json!({})))
        .await;
    assert!(message.contains("outside the node"), "{message}");
    rig.refused("DELETE", &format!("session/{SESSION}/share"), None)
        .await;

    let message = rig
        .refused("POST", &format!("session/{SESSION}/fork"), Some(json!({})))
        .await;
    assert!(message.contains("supervise"), "{message}");
    rig.refused("POST", &format!("session/{SESSION}/init"), Some(json!({})))
        .await;
    let message = rig
        .refused(
            "POST",
            &format!("api/session/{SESSION}/agent"),
            Some(json!({ "agent": "build" })),
        )
        .await;
    assert!(message.contains("agent"), "{message}");
}

/// A revert rewrites the working tree, so it is decided rather than forwarded:
/// a channel that denies it gets a refusal with nothing sent, and one that
/// allows it gets the call *and* an event saying the tree moved — because a
/// candidate review is bound to the tree that was captured (#188), and an
/// invalidation nobody can see is not an invalidation.
#[tokio::test]
async fn a_revert_is_decided_by_policy_and_a_permitted_one_says_the_tree_moved() {
    let denied = Rig::new().await;
    deny(&denied, "opencode_api", "revert");
    let (status, answer) = denied
        .call(
            "POST",
            &format!("session/{SESSION}/revert"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
    assert!(denied.requests().is_empty(), "{:?}", denied.requests());
    assert_eq!(denied.events_of("policy_denied").len(), 1);
    assert!(
        denied.events_of("workspace_changed").is_empty(),
        "nothing moved: nothing was sent"
    );

    let allowed = Rig::new().await;
    allow(&allowed, "opencode_api", "revert");
    let (status, answer) = allowed
        .call(
            "POST",
            &format!("session/{SESSION}/revert"),
            Some(json!({ "messageID": "msg_1" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let seen = allowed.requests();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(seen[0].path.ends_with("/revert"));
    assert_eq!(allowed.events_of("policy_allowed").len(), 1);
    let moved = allowed.events_of("workspace_changed");
    assert_eq!(moved.len(), 1, "{moved:?}");
    assert_eq!(moved[0]["action"], "revert");
    // Acting on it — invalidating a verified candidate whose tree this was —
    // is #188's, not the gateway's. What the gateway owes is the fact.
}

/// `POST /pty` is arbitrary command execution with no permission check of its
/// own (finding 7). Default-deny; an operator grant bound to this session and
/// workspace is what opens it; and the upgrade still needs a ticket the node
/// minted, so a granted capability alone does not open a socket.
#[tokio::test]
async fn a_pty_is_refused_by_default_and_spawns_nothing() {
    let rig = Rig::new().await;
    for (method, tail) in [
        ("POST", "pty"),
        ("POST", "api/pty"),
        ("GET", "pty/pty_1"),
        ("DELETE", "pty/pty_1"),
        ("POST", "pty/pty_1/connect-token"),
    ] {
        let message = rig
            .refused(method, tail, Some(json!({ "command": "/bin/bash" })))
            .await;
        assert!(message.contains("terminal"), "{method} /{tail}: {message}");
    }

    // Granting the capability does not bypass the rest of the mediation: the
    // spawn is still rewritten, and a command that is not one of the
    // workspace's own shells is still refused with nothing spawned. (The
    // shells list itself is read from the harness to decide that, which is why
    // this is not asserted through `refused`.)
    grant_terminal(&rig);
    rig.forget_requests();
    let (status, answer) = rig
        .call("POST", "pty", Some(json!({ "command": "/usr/bin/env" })))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
    assert!(
        answer["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("shells"),
        "{answer}"
    );
    assert!(
        rig.requests().iter().all(|r| r.method == "GET"),
        "a refused spawn must not have been sent: {:?}",
        rig.requests()
    );

    rig.forget_requests();
    let (status, _) = rig
        .call("POST", "pty", Some(json!({ "command": "/bin/bash" })))
        .await;
    assert_eq!(status, StatusCode::OK);
    // The shells list and the create: the gateway asks the image what it may
    // open a terminal with rather than taking the caller's word.
    assert_eq!(
        rig.requests()
            .iter()
            .map(|r| format!("{} {}", r.method, r.path))
            .collect::<Vec<_>>(),
        vec!["GET /pty/shells", "POST /pty"]
    );

    // And the upgrade is refused without a ticket the node minted, with
    // nothing reaching the harness.
    rig.forget_requests();
    let (status, _) = rig.call("GET", "pty/pty_1/connect", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(rig.requests().is_empty());
}

// ---------------------------------------------------------------------------
// 5. A mutation whose answer never comes back
// ---------------------------------------------------------------------------

/// The seam #196 left open, closed: a mediated mutation writes its intent
/// *before* it is dispatched, so a call the harness takes and never reports is
/// neither lost nor guessed at. The session is uncertain until reconciliation
/// asks the harness what it actually holds — and, finding the harness no
/// longer waiting, settles the intent without sending a second answer.
#[tokio::test]
async fn a_mediated_mutation_that_never_reports_is_uncertain_until_it_is_settled() {
    let rig = Rig::new().await;
    rig.fake.pending_permission(PERMISSION, "call_1");
    rig.fake.reply_hangs();

    let (status, answer) = rig
        .call(
            "POST",
            &format!("api/session/{SESSION}/permission/{PERMISSION}/reply"),
            Some(json!({ "reply": "once" })),
        )
        .await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{answer}");

    // The harness has the answer. The node cannot know that, and says so
    // rather than either claiming it or re-sending it.
    let replies = rig.seen.lock().unwrap().replies.clone();
    assert_eq!(replies.len(), 1, "{replies:?}");
    let unsettled = rig
        .store
        .opencode_unsettled_intents(TRACON_SESSION)
        .unwrap();
    assert_eq!(unsettled.len(), 1, "{unsettled:?}");
    assert_eq!(unsettled[0].state, intent_state::UNCERTAIN);
    assert_eq!(unsettled[0].target.as_deref(), Some(PERMISSION));
    assert_eq!(unsettled[0].detail.as_deref(), Some("allow_once"));
    assert!(
        unsettled[0]
            .note
            .as_deref()
            .unwrap_or_default()
            .contains("did not answer in time"),
        "{:?}",
        unsettled[0].note
    );
    assert!(rig
        .store
        .opencode_session_of(TRACON_SESSION)
        .unwrap()
        .unwrap()
        .is_uncertain());
    let doubt = rig.events_of("uncertain");
    assert_eq!(doubt.len(), 1, "{doubt:?}");
    assert_eq!(doubt[0]["refusing"], "prompt");

    // Reconciliation asks. The harness is no longer waiting on that request —
    // it took the answer — so the intent is settled and the doubt lifts, with
    // no second reply on the wire.
    rig.fake.reply_answers();
    let (commands, _rx) = tokio::sync::mpsc::channel(16);
    let ingest = Ingest::new(
        rig.store.clone(),
        Bus::new(),
        "n1".into(),
        TRACON_SESSION.into(),
        Instant::now(),
        commands,
        NativeEvents::new(),
    );
    ingest.rebind(SESSION, HttpApi::connect(rig.endpoint, PASSWORD));
    let report = ingest.reconcile(Reconcile::Reconnect).await;
    assert_eq!(report.settled, 1, "{report:?}");
    assert_eq!(
        report.resent, 0,
        "an answer the harness has must not be re-sent"
    );
    assert!(!ingest.is_uncertain());
    let settled = rig
        .store
        .opencode_intent(&unsettled[0].id)
        .unwrap()
        .unwrap();
    assert_eq!(settled.state, intent_state::ADMITTED);
    assert_eq!(
        rig.seen.lock().unwrap().replies.len(),
        1,
        "reconciliation sent the answer again"
    );
    assert!(rig
        .events_of("uncertain")
        .iter()
        .any(|e| e["cleared"].is_string()));
}

// ---------------------------------------------------------------------------
// 6 and 7. The pinned binary: two live sessions, and the fence
// ---------------------------------------------------------------------------

/// Two real `opencode serve` processes side by side. Per-session `HOME`, the
/// four `XDG_*` directories and `OPENCODE_DB` are what keep their state apart
/// (`config-state.md` §9 row 6); the gateway's mount is what keeps one
/// session's operator routes off the other's; and nothing either does ends the
/// other.
#[tokio::test]
async fn two_live_sessions_cannot_read_or_migrate_each_others_state() {
    state::isolate();
    let Some(binary) = pinned_binary() else {
        return;
    };
    let root = state::scratch("opencode-adversarial-two-sessions");

    let (a, b) = tokio::join!(
        Live::start(&binary, &root, "a"),
        Live::start(&binary, &root, "b")
    );

    // Two databases and two of every XDG directory. Not merely configured: on
    // disk, distinct, and each with a database in it. (`HOME` itself is
    // created lazily — only a tool that writes there makes it — which is why
    // the four XDG directories rather than the home are what is asserted:
    // they are the ones OpenCode creates at import, `config-state.md` §7.1.)
    assert_ne!(a.db(), b.db());
    assert!(!a.run().starts_with(b.run()) && !b.run().starts_with(a.run()));
    for live in [&a, &b] {
        assert!(live.db().is_file(), "{:?} has no database", live.db());
        for dir in ["config", "data", "cache", "state"] {
            assert!(
                live.run().join(dir).is_dir(),
                "{dir} is missing under {:?}",
                live.run()
            );
        }
    }
    assert!(
        !a.tree_mentions(b.upstream()),
        "session a's state names session b's session"
    );

    // And neither process holds a lock on its own database, let alone the
    // other's: this is finding 17 observed rather than read, and it is the
    // whole reason the node owns a fence of its own (row 6b). If this ever
    // starts failing, upstream has grown a lock and the fence can be revisited.
    for live in [&a, &b] {
        let db = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(live.db())
            .expect("the database opens while the harness is running");
        let taken = unsafe {
            libc::flock(
                std::os::unix::io::AsRawFd::as_raw_fd(&db),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        };
        assert_eq!(
            taken,
            0,
            "the harness holds an advisory lock on {:?}; upstream had none when this was written",
            live.db()
        );
        unsafe { libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(&db), libc::LOCK_UN) };
    }

    // A's server is its own instance: its session list holds its session and
    // not B's, whatever the credential could reach if the two shared one.
    let listed = a.get("/session").await;
    let ids: Vec<&str> = listed
        .as_array()
        .map(|rows| rows.iter().filter_map(|r| r["id"].as_str()).collect())
        .unwrap_or_default();
    assert!(ids.contains(&a.upstream()), "{listed}");
    assert!(
        !ids.contains(&b.upstream()),
        "session a's server can see session b's session: {listed}"
    );

    // And through the gateway, where the path is the only thing that could
    // name the other: A's mount refuses B's id rather than forwarding it.
    let (status, answer) = a
        .gateway("GET", &format!("api/session/{}", b.upstream()))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
    let (status, answer) = a
        .gateway("GET", &format!("api/session/{}", a.upstream()))
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");

    // Stopping one leaves the other running: they share no process, no port
    // and no lock.
    a.stop().await;
    let healthy = b.get("/global/health").await;
    assert_eq!(healthy["healthy"], true, "{healthy}");
    assert_eq!(healthy["version"], OpenCodeAdapter::PINNED_VERSION);
    b.stop().await;
}

/// What the harness leaves behind after a session, asked of the pinned binary
/// itself: no credential written into the runner's own auth store (an `oauth`
/// record would arm a subscription plugin inside the runner, a `wellknown` one
/// would fetch a config and run a command to mint a credential — finding 9),
/// and a configuration that is the node's and only the node's.
#[tokio::test]
async fn the_runners_own_auth_store_stays_empty_and_the_config_is_the_nodes() {
    state::isolate();
    let Some(binary) = pinned_binary() else {
        return;
    };
    let root = state::scratch("opencode-adversarial-auth-store");
    let live = Live::start(&binary, &root, "only").await;

    // Drive a session's worth of readable calls through the gateway, so this
    // is the state after use rather than after launch.
    for tail in ["global/health", "config", "session", "api/permission/saved"] {
        let (status, _) = live.gateway("GET", tail).await;
        assert_eq!(status, StatusCode::OK, "GET /{tail}");
    }

    // A PTY the capability does not cover is refused, and the proof that
    // nothing was spawned is the harness's own list.
    let (status, answer) = live
        .gateway_body(
            "POST",
            "pty",
            json!({ "command": "sh", "args": ["-c", "id"] }),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
    let ptys = live.get("/pty").await;
    assert_eq!(
        ptys.as_array().map(Vec::len).unwrap_or_default(),
        0,
        "a refused PTY spawned something: {ptys}"
    );

    // Every auth store the harness could have written, empty. The node's own
    // seeded file says `{}` and nothing added to it; no `oauth` or `wellknown`
    // record exists anywhere under this session's home.
    for file in ["auth.json", "mcp-auth.json"] {
        let written = live.run().join("data/opencode").join(file);
        if written.exists() {
            let body = std::fs::read_to_string(&written).unwrap();
            let parsed: Value = serde_json::from_str(&body).unwrap_or(json!({}));
            assert_eq!(parsed, json!({}), "{file} holds a credential: {body}");
        }
    }
    assert!(
        !live.tree_mentions("oauth") && !live.tree_mentions("wellknown"),
        "the runner's state names a credential record"
    );

    // And the configuration the server reports is the one the node wrote.
    let config = live.get("/config").await;
    assert_eq!(config["share"], "disabled", "{config}");
    assert_eq!(config["permission"]["*"], "ask", "{config}");
    live.stop().await;
}

/// A real `opencode serve` killed under a mutation. The node cannot know
/// whether the harness acted before it died, so it says so: the intent written
/// before dispatch is marked uncertain with the reason, the session with it,
/// and the next prompt is refused rather than duplicating a turn that may
/// already have run. Nothing here guesses, and nothing is silently dropped.
#[tokio::test]
async fn a_harness_killed_under_a_mutation_leaves_the_intent_uncertain() {
    state::isolate();
    let Some(binary) = pinned_binary() else {
        return;
    };
    let root = state::scratch("opencode-adversarial-killed");
    let live = Live::start(&binary, &root, "killed").await;
    let healthy = live.get("/global/health").await;
    assert_eq!(healthy["healthy"], true, "{healthy}");

    live.kill().await;

    // A mediated mutation into a server that is no longer there.
    let (status, answer) = live
        .gateway_body(
            "POST",
            &format!("api/session/{}/compact", live.upstream()),
            json!({}),
        )
        .await;
    assert!(
        status == StatusCode::BAD_GATEWAY || status == StatusCode::GATEWAY_TIMEOUT,
        "a mutation into a dead harness answered {status}: {answer}"
    );

    let unsettled = live
        .store
        .opencode_unsettled_intents(&live.session_id)
        .unwrap();
    assert_eq!(unsettled.len(), 1, "{unsettled:?}");
    assert_eq!(unsettled[0].state, intent_state::UNCERTAIN);
    assert!(
        unsettled[0]
            .note
            .as_deref()
            .unwrap_or_default()
            .contains("compact"),
        "the note must name the mutation: {:?}",
        unsettled[0].note
    );
    let session = live
        .store
        .opencode_session_of(&live.session_id)
        .unwrap()
        .unwrap();
    assert!(session.is_uncertain());
    assert!(
        session
            .uncertain_reason
            .as_deref()
            .unwrap_or_default()
            .contains("unreachable")
            || session
                .uncertain_reason
                .as_deref()
                .unwrap_or_default()
                .contains("in time"),
        "{session:?}"
    );
    let doubt = live.events_of("uncertain");
    assert_eq!(doubt.len(), 1, "{doubt:?}");
    assert_eq!(doubt[0]["refusing"], "prompt");
    live.stop().await;
}

/// The fence upstream does not have (`config-state.md` §7.2, verdict row 6b:
/// two processes can open the same database and nothing prevents it). The node
/// owns it, keyed to the session whose state it is, and it is the launch path
/// that takes it.
#[test]
fn a_second_writer_on_one_sessions_state_is_refused_by_the_node() {
    state::isolate();
    let session = "adversarial-single-writer";
    tracon::session::materialize::release_state(session);
    tracon::session::materialize::claim_state(session).expect("the first writer takes it");
    let refused = tracon::session::materialize::claim_state(session)
        .expect_err("a second writer must be refused");
    assert_eq!(refused.kind(), std::io::ErrorKind::WouldBlock);
    assert!(refused.to_string().contains("already open for writing"));
    tracon::session::materialize::release_state(session);
    tracon::session::materialize::claim_state(session).expect("a released fence is free");
    tracon::session::materialize::release_state(session);
}

// ---------------------------------------------------------------------------
// Driving the real binary
// ---------------------------------------------------------------------------

/// One live `opencode serve` started the way the adapter starts one, with the
/// operator router and this session's gateway mount in front of it.
struct Live {
    container: String,
    root: std::path::PathBuf,
    app: axum::Router,
    store: Arc<Store>,
    session_id: String,
    api: NativeApi,
    handle: Option<Box<dyn HarnessHandle>>,
    runner: LiveRunner,
    http: reqwest::Client,
}

impl Live {
    async fn start(binary: &str, root: &std::path::Path, name: &str) -> Self {
        let root = root.join(name);
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        // Ambient configuration the harness must not read, planted in the
        // workspace of each session so a leak between them is visible.
        std::fs::write(
            work.join("opencode.json"),
            format!(r#"{{ "share": "auto", "permission": {{ "*": "allow" }}, "model": "planted-{name}/planted" }}"#),
        )
        .unwrap();

        let adapter = OpenCodeAdapter::new(OpenCodeAdapter::PINNED_VERSION);
        let wiring = live_wiring();
        let state = root.join(".opencode");
        std::fs::create_dir_all(state.join("run")).unwrap();
        for (file, body) in adapter.scratch_files(&wiring) {
            std::fs::write(state.join(&file), body).unwrap();
        }

        let container = format!("tracon-opencode-adv-{}-{name}", std::process::id());
        let runner = LiveRunner {
            binary: binary.to_string(),
            launched: Arc::new(Mutex::new(None)),
        };
        let spec = LaunchSpec {
            cwd_in_runner: work.to_string_lossy().into_owned(),
            model: "anthropic/claude-x".into(),
            container_name: container.clone(),
            harness_home: root.to_string_lossy().into_owned(),
            ..spec()
        };
        let (handle, _events) = adapter
            .launch(&runner, spec)
            .await
            .expect("the pinned binary starts");
        let api = handle.native_api().expect("a native API");

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
        let session_id = format!("s-live-{name}");
        let mut row = support::rows::session_row(&session_id, "n1", "personal");
        row.harness_id = "opencode".into();
        row.harness_session_id = Some(api.session_id.clone());
        store.insert_session(&row).unwrap();
        // The identity map a running session has, so a mutation's doubt lands
        // on the same row ingestion would read it from.
        store
            .opencode_bind(&session_id, &api.session_id, None)
            .unwrap();
        manager
            .register_native_api_for_test(&session_id, api.clone())
            .await;
        let app = tracon::http::router(AppState {
            manager: manager.clone(),
            cfg,
            adapter: Arc::new(FakeAdapter {
                tx: Arc::new(tokio::sync::Mutex::new(None)),
                tokens: Arc::new(tokio::sync::Mutex::new(0)),
            }),
            node_id: "n1".into(),
            tools,
            mesh: None,
            auth: Arc::new(AuthState::new("127.0.0.1".into(), None)),
            enroll: Default::default(),
        });

        Self {
            container,
            root,
            app,
            store,
            session_id,
            api,
            handle: Some(handle),
            runner,
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
        }
    }

    /// Every event this session's log holds of one kind.
    fn events_of(&self, kind: &str) -> Vec<Value> {
        self.store
            .events_after(&self.session_id, 0, 500)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == kind)
            .map(|e| e.payload)
            .collect()
    }

    /// Kill the server the way the node kills a harness: the process group
    /// this launch was remembered under, by pid, not by pattern.
    async fn kill(&self) {
        self.runner.kill(&self.container).await.unwrap();
        // Wait until the endpoint really is dead before anything is asked of
        // it. The port was an ephemeral one the launch picked by binding zero,
        // so a test that asked the gateway on the assumption that a killed
        // process leaves its port unanswered could be answered by whatever
        // took the port next.
        for _ in 0..200 {
            let alive = self
                .http
                .get(format!("{}/global/health", self.api.base))
                .header("authorization", &self.api.authorization)
                .timeout(std::time::Duration::from_millis(500))
                .send()
                .await
                .is_ok();
            if !alive {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("the killed harness is still answering on {}", self.api.base);
    }

    fn upstream(&self) -> &str {
        &self.api.session_id
    }

    /// This session's writable tree: the home and the four XDG directories the
    /// launch environment names.
    fn run(&self) -> std::path::PathBuf {
        self.root.join(".opencode/run")
    }

    fn db(&self) -> std::path::PathBuf {
        self.run().join("state/opencode.db")
    }

    /// Whether anything under this session's own state names `needle`. Used
    /// for the questions that are about absence: another session's id, a
    /// credential record.
    fn tree_mentions(&self, needle: &str) -> bool {
        fn walk(dir: &std::path::Path, needle: &str) -> bool {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return false;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if walk(&path, needle) {
                        return true;
                    }
                } else if let Ok(body) = std::fs::read(&path) {
                    if String::from_utf8_lossy(&body).contains(needle) {
                        return true;
                    }
                }
            }
            false
        }
        walk(&self.run(), needle)
    }

    /// Straight to the harness, as the node itself talks to it.
    async fn get(&self, path: &str) -> Value {
        let url = format!(
            "{}{path}{}directory={}",
            self.api.base,
            if path.contains('?') { "&" } else { "?" },
            self.api.directory
        );
        let response = self
            .http
            .get(url)
            .header("authorization", &self.api.authorization)
            .send()
            .await
            .expect("the harness answers");
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        assert!(status.is_success(), "{path}: {status} {text}");
        serde_json::from_str(&text).unwrap_or(Value::String(text))
    }

    async fn gateway(&self, method: &str, tail: &str) -> (StatusCode, Value) {
        self.request(method, tail, None).await
    }

    async fn gateway_body(&self, method: &str, tail: &str, body: Value) -> (StatusCode, Value) {
        self.request(method, tail, Some(body)).await
    }

    async fn request(&self, method: &str, tail: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut b = Request::builder()
            .method(method)
            .uri(format!("/api/opencode/{}/{tail}", self.session_id))
            .header("host", "127.0.0.1:7420");
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
            .insert(ConnectInfo(LOCAL.parse::<SocketAddr>().unwrap()));
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes).to_string();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn stop(mut self) {
        if let Some(handle) = self.handle.take() {
            handle.close().await.ok();
        }
        let _ = self.runner.kill(&self.container).await;
    }
}

/// The provider wiring a live launch gets: pointed at a gateway host that does
/// not resolve, because nothing in these tests calls a model and an accidental
/// call must fail rather than reach anything.
fn live_wiring() -> tracon::gateway::model::Wiring {
    let mut cfg = Config::default();
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
    tracon::gateway::model::harness_wiring(&cfg, "tracon-gw", "session-token", |_, _| true)
}

/// The pinned binary, when this machine has it: named in
/// `TRACON_OPENCODE_BINARY`, or on `PATH`. A build at another version proves
/// nothing about the release this gateway was written against, so it is
/// skipped rather than run.
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
/// the adapter would do inside a container.
struct LiveRunner {
    binary: String,
    launched: Arc<Mutex<Option<String>>>,
}

#[async_trait::async_trait]
impl Runner for LiveRunner {
    async fn spawn(&self, mut cmd: RunnerCommand) -> Result<Spawned, RunnerError> {
        cmd.argv[0] = self.binary.clone();
        let spawned = tracon::runner::local::LocalRunner.spawn(cmd).await?;
        if let Some(endpoint) = spawned.endpoint.clone() {
            *self.launched.lock().unwrap() = Some(endpoint);
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
        tracon::runner::local::LocalRunner.kill(name).await
    }
}
