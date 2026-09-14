//! Gate E, in one process: a request on node A for a session node B owns,
//! answered by B over a bounded encrypted stream the hub only relays.
//!
//! Everything is asserted from both ends — what A's operator got back, and
//! what actually reached B's harness — because a stream that answers
//! plausibly without the harness having been asked would pass a test written
//! only against the response.

#[path = "support/mod.rs"]
mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::StreamExt;
use proto::envelope::DataKey;
use proto::frame::MESH_CHANNEL;
use proto::keyring::Keyring;
use proto::keys::Identity;
use proto::stream::{
    new_stream_id, refused, Direction, StreamEnvelope, StreamFrame, StreamKind, StreamOpen,
    STREAM_PROTOCOL_VERSION,
};
use serde_json::{json, Value};
use tower::ServiceExt;

use support::fake_opencode::{serve, Fake, Seen, PERMISSION, SESSION};
use support::mesh::{identity, wait_for};
use support::state;
use tracon::adapter::NativeApi;
use tracon::config::Config;
use tracon::http::api::AppState;
use tracon::mesh::client::MeshClient;
use tracon::session::Manager;
use tracon::store::Store;
use tracon::stream::Bus;

const PASSWORD: &str = "the-node-minted-this";
const WORKSPACE: &str = "/work";
const CHANNEL: &str = "personal";
/// The tracon session id both nodes know the session by.
const TRACON_SESSION: &str = "s-remote";

struct Node {
    id: Identity,
    app: axum::Router,
    store: Arc<Store>,
    client: Arc<MeshClient>,
    manager: Manager,
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

async fn node(seed: u8, name: &str, hub: &str, rings: &[(&str, Keyring)]) -> Node {
    let id = identity(seed);
    let store = Arc::new(Store::open_in_memory().unwrap());
    store
        .put_node(&{
            let mut r = support::rows::node_row(&id.node_id(), name);
            r.x25519_pub = Some(id.x25519_hex());
            r
        })
        .unwrap();
    for (c, ring) in rings {
        store.channel_put(c, &ring.to_bytes(), "{}").unwrap();
        store.node_channel_add(&id.node_id(), c).unwrap();
    }
    let bus = Bus::new();
    let mut cfg = Config::default();
    cfg.mesh.hub_url = Some(hub.to_string());
    cfg.mesh.poll_secs = 1;
    cfg.mesh.heartbeat_secs = 5;
    cfg.mesh.stream_open_timeout_secs = 10;
    cfg.mesh.stream_idle_secs = 5;
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
        bus.clone(),
        cfg.clone(),
        id.node_id(),
        tools.clone(),
        Default::default(),
        Arc::new(tracon::runner::local::LocalBackend),
    );
    let client = MeshClient::new(
        identity(seed),
        hub,
        store.clone(),
        bus.clone(),
        cfg.clone(),
        Default::default(),
    );
    bus.with_tap(client.spawn());
    manager.set_mesh(client.clone());
    let state = AppState {
        manager: manager.clone(),
        cfg,
        adapter: Arc::new(support::fake::FakeAdapter {
            tx: Arc::new(tokio::sync::Mutex::new(None)),
            tokens: Arc::new(tokio::sync::Mutex::new(0)),
        }),
        node_id: id.node_id(),
        tools,
        mesh: Some(client.clone()),
        auth: Arc::new(tracon::http::auth::AuthState::new("127.0.0.1".into(), None)),
        enroll: Default::default(),
    };
    client.set_executor(Arc::new(state.clone()));
    client.streams().set_executor(Arc::new(state.clone()));
    Node {
        id,
        app: tracon::http::router(state),
        store,
        client,
        manager,
    }
}

struct Pair {
    a: Node,
    b: Node,
    hub: support::mesh::TestHub,
    seen: Arc<std::sync::Mutex<Seen>>,
    fake: Fake,
}

/// Two nodes on one hub, both in `@mesh` and `personal`, with a session that
/// runs on B and is known to A as B's.
async fn pair(cut_after: usize) -> Pair {
    state::isolate();
    let a_id = identity(41);
    let b_id = identity(42);
    let channels: &[&str] = &[MESH_CHANNEL, CHANNEL];
    let hub = support::mesh::start_hub_managed(&[(&a_id, channels), (&b_id, channels)], 0).await;

    let mesh = Keyring::genesis(&a_id.x25519_public(), &DataKey::generate());
    let personal = Keyring::genesis(&a_id.x25519_public(), &DataKey::generate());
    let b_mesh = mesh.wrap_for(&a_id, &b_id.x25519_public()).unwrap();
    let b_personal = personal.wrap_for(&a_id, &b_id.x25519_public()).unwrap();

    let a = node(
        41,
        "alpha",
        &hub.url,
        &[(MESH_CHANNEL, mesh), (CHANNEL, personal)],
    )
    .await;
    let b = node(
        42,
        "beta",
        &hub.url,
        &[(MESH_CHANNEL, b_mesh), (CHANNEL, b_personal)],
    )
    .await;

    // The harness B's session is running, and the session row on both nodes:
    // owned by B, mirrored on A.
    let fake = Fake::new("1.18.30", cut_after);
    let seen = fake.seen.clone();
    *fake.password.lock().unwrap() = PASSWORD.to_string();
    // Nothing on the durable stream is published until a prompt is admitted;
    // this test drives the gateway, not the adapter, so say so up front.
    fake.prompted
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let endpoint = serve(fake.clone()).await;

    for n in [&a, &b] {
        let mut row = support::rows::session_row(TRACON_SESSION, &b.id.node_id(), CHANNEL);
        row.harness_id = "opencode".into();
        row.harness_session_id = Some(SESSION.into());
        n.store.ensure_peer_node(&a.id.node_id()).unwrap();
        n.store.ensure_peer_node(&b.id.node_id()).unwrap();
        n.store.insert_session(&row).unwrap();
    }
    b.manager
        .register_native_api_for_test(TRACON_SESSION, native_api(endpoint))
        .await;

    // Presence, and each node's own record of who holds which channel: the
    // owner's authorization is read from the latter, never from the opener.
    a.client.hello().await.unwrap();
    b.client.hello().await.unwrap();
    a.client.refresh_members().await.unwrap();
    b.client.refresh_members().await.unwrap();
    let (ai, bi) = (a.id.node_id(), b.id.node_id());
    wait_for("A to see B reachable", || {
        a.store
            .get_node(&bi)
            .ok()
            .flatten()
            .is_some_and(|n| n.reachable == 1)
    })
    .await;
    wait_for("B to see A reachable", || {
        b.store
            .get_node(&ai)
            .ok()
            .flatten()
            .is_some_and(|n| n.reachable == 1)
    })
    .await;
    // Both relay connections are up before anything is asked of them.
    wait_for("the relay to hold both connections", || {
        hub.relay_connected(&ai) && hub.relay_connected(&bi)
    })
    .await;

    Pair {
        a,
        b,
        hub,
        seen,
        fake,
    }
}

/// One request on a node's operator API, answered as bytes rather than as
/// JSON, so a streamed body can be read as it arrives.
async fn raw(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, axum::http::HeaderMap, axum::body::Body) {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "127.0.0.1:7420");
    if body.is_some() {
        b = b.header("content-type", "application/json");
    }
    let req = b
        .body(match body {
            Some(v) => Body::from(v.to_string()),
            None => Body::empty(),
        })
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    (status, headers, res.into_body())
}

async fn collect(body: Body) -> Vec<u8> {
    axum::body::to_bytes(body, 8 << 20)
        .await
        .map(|b| b.to_vec())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------

/// The exit criterion: a readable GET and a mediated POST for a session on B,
/// made on A, reaching B's harness with B's own credential and coming back.
#[tokio::test]
async fn http_round_trips_through_the_relay_in_both_directions() {
    let p = pair(usize::MAX).await;

    let (status, headers, body) = raw(
        &p.a.app,
        "GET",
        &format!("/api/opencode/{TRACON_SESSION}/api/session/{SESSION}/message"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("tracon-served-by").map(|v| v.to_str().unwrap()),
        Some("owner-stream"),
        "the answer says where it came from"
    );
    let value: Value = serde_json::from_slice(&collect(body).await).unwrap();
    assert!(value["data"].is_array(), "{value}");

    // It reached B's harness, with B's credential injected there and A's
    // cookie nowhere near it.
    {
        let seen = p.seen.lock().unwrap();
        let hit = seen
            .requests
            .iter()
            .find(|r| r.path.ends_with("/message"))
            .expect("the harness saw the read");
        assert_eq!(hit.method, "GET");
        assert!(hit.authorization.as_deref().unwrap().starts_with("Basic "));
        assert_eq!(hit.cookie, None);
        assert_eq!(hit.directory_header.as_deref(), Some(WORKSPACE));
        assert!(
            hit.query.contains("directory=/work"),
            "the directory is pinned by the owner: {}",
            hit.query
        );
    }

    // A mutation: mediated on B, narrowed there, and answered back over the
    // same family. The body travelled as chunks and was checked by digest.
    let (status, _, body) = raw(
        &p.a.app,
        "POST",
        &format!(
            "/api/opencode/{TRACON_SESSION}/api/session/{SESSION}/permission/{PERMISSION}/reply"
        ),
        Some(json!({ "reply": "always" })),
    )
    .await;
    let answer = collect(body).await;
    assert!(
        status.is_success(),
        "{status}: {}",
        String::from_utf8_lossy(&answer)
    );
    let reply = {
        let seen = p.seen.lock().unwrap();
        seen.replies.last().cloned().expect("the harness was told")
    };
    assert_eq!(reply["id"], PERMISSION);
    assert_eq!(
        reply["body"]["reply"], "once",
        "the owner narrowed it, as it does for a local operator"
    );
    let _ = p.fake;
}

/// An event stream is chunks until the owner closes it, not a buffered answer.
#[tokio::test]
async fn an_event_stream_flows_until_the_owner_closes_it() {
    // The fake cuts the durable stream after two events, which is the close
    // this end has to see rather than hang on.
    let p = pair(2).await;
    let (status, headers, body) = raw(
        &p.a.app,
        "GET",
        &format!("/api/opencode/{TRACON_SESSION}/api/session/{SESSION}/event"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default(),
        "text/event-stream"
    );
    let mut stream = body.into_data_stream();
    let mut text = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(5), stream.next()).await {
            Ok(Some(Ok(bytes))) => text.push_str(&String::from_utf8_lossy(&bytes)),
            // The owner's harness cut the stream; the body ends here, which is
            // exactly the close the serving side must propagate.
            Ok(Some(Err(_))) | Ok(None) => break,
            Err(_) => break,
        }
        if text.matches("data:").count() >= 2 {
            break;
        }
    }
    assert!(
        text.contains("data:"),
        "events reached the operator through the relay: {text:?}"
    );
}

/// The hub routes and bounds; it never holds a key, and it stores nothing.
#[tokio::test]
async fn the_relay_carries_ciphertext_and_keeps_nothing() {
    let p = pair(usize::MAX).await;
    let (status, _, body) = raw(
        &p.a.app,
        "GET",
        &format!("/api/opencode/{TRACON_SESSION}/api/session/{SESSION}/message"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let _ = collect(body).await;

    // Nothing a stream carried was appended to any channel: the durable store
    // holds only the hellos, and none of them names the session.
    let mut durable = String::new();
    for channel in [MESH_CHANNEL, CHANNEL] {
        let page = hub::store::FrameStore::read(&*p.hub.frames, channel, 0, 1000).unwrap();
        for (_, frame) in page.frames {
            durable.push_str(&frame);
        }
    }
    assert!(
        !durable.contains(TRACON_SESSION) && !durable.contains(SESSION),
        "a stream must not reach the durable store"
    );
    // And the relay is holding nothing once the request is answered.
    wait_for("the relay to forget the stream", || {
        p.hub.state.streams.open_streams() == 0
    })
    .await;
}

/// The owner's own record of who holds a channel is the authorization. A node
/// the serving side is happy with is refused here if this node does not grant
/// it the session's channel — and open streams end rather than finishing.
#[tokio::test]
async fn an_opener_without_authority_on_the_owner_is_refused() {
    let p = pair(usize::MAX).await;
    let ai = p.a.id.node_id();
    // A is still a member on the hub and still holds the channel key; only
    // B's record of A's channels is cleared. The refusal has to come from
    // B's own check, not from the relay or the key.
    p.b.store.node_channels_set(&ai, &[]).unwrap();

    let (status, _, body) = raw(
        &p.a.app,
        "GET",
        &format!("/api/opencode/{TRACON_SESSION}/api/session/{SESSION}/message"),
        None,
    )
    .await;
    let text = String::from_utf8_lossy(&collect(body).await).to_string();
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    assert!(text.contains("not a member of channel"), "{text}");
    // Nothing reached the harness on that attempt.
    let seen = p.seen.lock().unwrap();
    assert!(
        !seen.requests.iter().any(|r| r.path.ends_with("/message")),
        "a refused stream must never reach the harness"
    );
}

/// A member the hub removed cannot open a stream at all: the relay refuses to
/// route to or from it, before either node is asked to be honest about it.
#[tokio::test]
async fn removing_a_member_stops_the_relay_routing_for_them() {
    let p = pair(usize::MAX).await;
    let ai = p.a.id.node_id();
    hub::store::MemberStore::remove(&*p.hub.members, &ai).unwrap();

    let (status, _, body) = raw(
        &p.a.app,
        "GET",
        &format!("/api/opencode/{TRACON_SESSION}/api/session/{SESSION}/message"),
        None,
    )
    .await;
    let text = String::from_utf8_lossy(&collect(body).await).to_string();
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{text}");
    let seen = p.seen.lock().unwrap();
    assert!(!seen.requests.iter().any(|r| r.path.ends_with("/message")));
}

/// A peer speaking a different stream protocol is refused by name, and the
/// refusal is the one the operator reads — not a timeout.
#[tokio::test]
async fn a_protocol_mismatch_is_refused_by_name() {
    let p = pair(usize::MAX).await;
    let open = StreamOpen {
        protocol_version: STREAM_PROTOCOL_VERSION + 1,
        session_id: TRACON_SESSION.into(),
        kind: StreamKind::Http,
        method: "GET".into(),
        path: format!("api/session/{SESSION}/message"),
        query: None,
        headers: Vec::new(),
        body_sha256: None,
        body_len: 0,
        operator: None,
        owner_epoch: None,
    };
    let answer = p.raw_open(open).await;
    match answer {
        StreamFrame::Refused { reason, detail } => {
            assert_eq!(reason, refused::VERSION);
            assert!(detail.unwrap().contains("owner-stream protocol"));
        }
        other => panic!("expected a refusal, got {}", other.name()),
    }
}

/// An open naming an owner epoch this node has moved past is fenced. The
/// serving node re-opens on the next request; it never resumes, and it never
/// re-sends a body.
#[tokio::test]
async fn an_owner_epoch_that_moved_is_fenced() {
    let p = pair(usize::MAX).await;
    let open = StreamOpen {
        protocol_version: STREAM_PROTOCOL_VERSION,
        session_id: TRACON_SESSION.into(),
        kind: StreamKind::Http,
        method: "GET".into(),
        path: format!("api/session/{SESSION}/message"),
        query: None,
        headers: Vec::new(),
        body_sha256: None,
        body_len: 0,
        operator: None,
        owner_epoch: Some("a-run-that-ended".into()),
    };
    match p.raw_open(open).await {
        StreamFrame::Refused { reason, .. } => assert_eq!(reason, refused::FENCED),
        other => panic!("expected a fence, got {}", other.name()),
    }
    // And the same open without the stale epoch is answered.
    let open = StreamOpen {
        protocol_version: STREAM_PROTOCOL_VERSION,
        session_id: TRACON_SESSION.into(),
        kind: StreamKind::Http,
        method: "GET".into(),
        path: format!("api/session/{SESSION}/message"),
        query: None,
        headers: Vec::new(),
        body_sha256: None,
        body_len: 0,
        operator: None,
        owner_epoch: Some(p.b.client.streams().owner_epoch().to_string()),
    };
    match p.raw_open(open).await {
        StreamFrame::Head { status, .. } => assert_eq!(status, 200),
        other => panic!("expected the answer, got {}", other.name()),
    }
}

/// A session that is not the owner's is not reachable through the owner,
/// whatever the opener says about it.
#[tokio::test]
async fn an_unknown_session_is_refused_rather_than_guessed() {
    let p = pair(usize::MAX).await;
    let open = StreamOpen {
        protocol_version: STREAM_PROTOCOL_VERSION,
        session_id: "s-not-here".into(),
        kind: StreamKind::Http,
        method: "GET".into(),
        path: format!("api/session/{SESSION}/message"),
        query: None,
        headers: Vec::new(),
        body_sha256: None,
        body_len: 0,
        operator: None,
        owner_epoch: None,
    };
    match p.raw_open(open).await {
        StreamFrame::Refused { reason, .. } => assert_eq!(reason, refused::UNKNOWN_SESSION),
        other => panic!("expected a refusal, got {}", other.name()),
    }
}

/// A POST whose stream dies is not re-sent. The operator is told the outcome
/// is unknown; nothing on this node decides to try again.
#[tokio::test]
async fn a_mutation_is_never_replayed_when_the_owner_restarts() {
    let p = pair(usize::MAX).await;
    // One successful mutation, so the serving node has learned B's epoch.
    let (status, _, body) = raw(
        &p.a.app,
        "POST",
        &format!(
            "/api/opencode/{TRACON_SESSION}/api/session/{SESSION}/permission/{PERMISSION}/reply"
        ),
        Some(json!({ "reply": "once" })),
    )
    .await;
    let _ = collect(body).await;
    assert!(status.is_success());
    assert_eq!(p.seen.lock().unwrap().replies.len(), 1);

    // B restarts: a new run, a new owner epoch. The next mutation from A
    // names the epoch that died, is fenced, and is **not** re-sent.
    p.b.client.streams().restart();
    let (status, _, body) = raw(
        &p.a.app,
        "POST",
        &format!(
            "/api/opencode/{TRACON_SESSION}/api/session/{SESSION}/permission/{PERMISSION}/reply"
        ),
        Some(json!({ "reply": "once" })),
    )
    .await;
    let text = String::from_utf8_lossy(&collect(body).await).to_string();
    assert_eq!(status, StatusCode::CONFLICT, "{text}");
    assert!(text.contains("never replayed"), "{text}");
    assert_eq!(
        p.seen.lock().unwrap().replies.len(),
        1,
        "the harness must not see the mutation twice"
    );

    // The fence cleared the remembered epoch, so the next request opens
    // afresh and is answered.
    let (status, _, body) = raw(
        &p.a.app,
        "GET",
        &format!("/api/opencode/{TRACON_SESSION}/api/session/{SESSION}/message"),
        None,
    )
    .await;
    let text = String::from_utf8_lossy(&collect(body).await).to_string();
    assert_eq!(status, StatusCode::OK, "{text}");
}

/// A consumer that stops reading does not cost the owner unbounded memory and
/// does not cost the stream a single byte. The credit window closes, the owner
/// waits, the relay drops nothing, and everything arrives when reading
/// resumes.
#[tokio::test]
async fn a_slow_consumer_stops_the_credits_rather_than_the_stream() {
    let p = pair(usize::MAX).await;
    let (status, _, body) = raw(
        &p.a.app,
        "GET",
        &format!("/api/opencode/{TRACON_SESSION}/api/session/{SESSION}/event"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let mut stream = body.into_data_stream();

    // Nothing is read while the harness keeps producing. The owner's window
    // closes after `INITIAL_CREDIT` chunks and it parks there.
    wait_for("the owner to run out of credit", || {
        p.b.client.streams().credit_waits() > 0
    })
    .await;
    assert_eq!(
        p.b.client
            .streams()
            .stats
            .relay_drops
            .load(std::sync::atomic::Ordering::Relaxed),
        0,
        "backpressure must not be a drop"
    );
    assert_eq!(
        p.hub.state.streams.open_streams(),
        1,
        "the relay is still carrying it"
    );

    // Reading resumes, and what was held comes through.
    let mut text = String::new();
    for _ in 0..4 {
        match tokio::time::timeout(Duration::from_secs(10), stream.next()).await {
            Ok(Some(Ok(bytes))) => text.push_str(&String::from_utf8_lossy(&bytes)),
            _ => break,
        }
    }
    assert!(
        !text.is_empty(),
        "the held bytes arrived rather than being dropped"
    );
}

/// The relay connection dropping ends every stream on it. Nothing is replayed
/// to make up for it: the next request opens a new stream, and a mutation
/// already sent is not sent again.
#[tokio::test]
async fn a_dropped_relay_connection_re_opens_without_replaying_a_mutation() {
    let p = pair(usize::MAX).await;
    let (status, _, body) = raw(
        &p.a.app,
        "POST",
        &format!(
            "/api/opencode/{TRACON_SESSION}/api/session/{SESSION}/permission/{PERMISSION}/reply"
        ),
        Some(json!({ "reply": "once" })),
    )
    .await;
    let _ = collect(body).await;
    assert!(status.is_success());
    assert_eq!(p.seen.lock().unwrap().replies.len(), 1);

    // The hub drops A's connection: a new one replaces it and the old
    // receiver goes, exactly as a network cut looks from here.
    let ai = p.a.id.node_id();
    let cut = {
        let doomed = p.hub.state.streams.connect(&ai);
        let epoch = p.hub.state.streams.epoch_of(&ai).unwrap();
        drop(doomed);
        epoch
    };
    // A's own connection ended when this one replaced it; it is back when the
    // relay is holding a connection newer than the one that cut it.
    wait_for("A to reconnect to the relay", || {
        p.hub.state.streams.epoch_of(&ai).is_some_and(|e| e > cut)
    })
    .await;

    // A read goes through on the new connection...
    let (status, _, body) = raw(
        &p.a.app,
        "GET",
        &format!("/api/opencode/{TRACON_SESSION}/api/session/{SESSION}/message"),
        None,
    )
    .await;
    let text = String::from_utf8_lossy(&collect(body).await).to_string();
    assert_eq!(status, StatusCode::OK, "{text}");
    // ...and the reconnect replayed nothing.
    assert_eq!(
        p.seen.lock().unwrap().replies.len(),
        1,
        "a reconnect must never re-send a mutation"
    );
}

// ---------------------------------------------------------------------------
// A peer that speaks the relay directly, for the frames no honest node sends.
// ---------------------------------------------------------------------------

impl Pair {
    /// Open a stream to B as A would, but with a frame this build's serving
    /// side would never construct, and return B's first answer.
    async fn raw_open(&self, open: StreamOpen) -> StreamFrame {
        let a = identity(41);
        let ring =
            Keyring::from_bytes(&self.a.store.channel_get(CHANNEL).unwrap().unwrap().keyring)
                .unwrap();
        let epoch = hex::encode(ring.newest().id());
        let stream_id = new_stream_id();
        let env = StreamEnvelope::seal(
            &a,
            CHANNEL,
            &self.b.id.node_id(),
            &ring,
            &epoch,
            &stream_id,
            0,
            Direction::Serving,
            &StreamFrame::Open(Box::new(open)),
            tracon::store::now_ms(),
        )
        .unwrap();

        // The answer comes back on A's own relay connection, so it is taken
        // off the hub directly here rather than through A's router.
        let rx = self.hub.state.streams.connect(&a.node_id());
        post_signed(
            &self.hub.url,
            &a,
            "/v0/streams",
            serde_json::to_vec(&env).unwrap(),
        )
        .await;
        let mut rx = rx;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while tokio::time::Instant::now() < deadline {
            let Ok(Some(delivery)) = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await
            else {
                break;
            };
            if delivery.event != "stream" {
                continue;
            }
            let Ok(back) = serde_json::from_str::<StreamEnvelope>(&delivery.data) else {
                continue;
            };
            if back.stream_id != stream_id {
                continue;
            }
            back.verify().unwrap();
            return back.open(&ring, &a, Direction::Owner).unwrap();
        }
        panic!("the owner never answered the raw open");
    }
}

async fn post_signed(hub: &str, id: &Identity, path: &str, body: Vec<u8>) {
    let ts = (tracon::store::now_ms() / 1000).max(0) as u64;
    let mut req = reqwest::Client::new()
        .post(format!("{hub}{path}"))
        .header("content-type", "application/json");
    for (k, v) in proto::auth::signed_headers(id, "POST", path, &body, ts) {
        req = req.header(k, v);
    }
    let res = req.body(body).send().await.unwrap();
    assert!(
        res.status().is_success(),
        "the relay refused the raw frame: {}",
        res.text().await.unwrap_or_default()
    );
}
