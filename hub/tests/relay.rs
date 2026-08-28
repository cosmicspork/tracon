//! Drives the hub router in-process over memory stores.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use hub::store::{Member, MemberStore, MemoryFrames, MemoryMembers};
use hub::{app, HubConfig};
use proto::auth::signed_headers;
use proto::envelope::DataKey;
use proto::frame::{Envelope, Payload, MESH_CHANNEL};
use proto::keyring::Keyring;
use proto::keys::Identity;
use serde_json::{json, Value};
use tower::ServiceExt;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

struct Hub {
    app: Router,
    members: Arc<MemoryMembers>,
}

fn hub_with(admitted: &[(&Identity, &[&str])]) -> Hub {
    let members = Arc::new(MemoryMembers::new());
    for (id, channels) in admitted {
        members
            .put(&Member {
                node_id: id.node_id(),
                x25519_pub: id.x25519_hex(),
                name: "n".into(),
                channels: channels.iter().map(|s| s.to_string()).collect(),
                admitted_ms: 0,
                admitted_by: "test".into(),
                role: Default::default(),
            })
            .unwrap();
    }
    let app = app(
        Arc::new(MemoryFrames::new()),
        members.clone(),
        HubConfig::default(),
    );
    Hub { app, members }
}

async fn call(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), 1 << 20).await.unwrap();
    let v = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, v)
}

fn signed(id: &Identity, method: &str, path: &str, body: &str, ts: u64) -> Request<Body> {
    let mut b = Request::builder().method(method).uri(path);
    for (k, v) in signed_headers(id, method, path, body.as_bytes(), ts) {
        b = b.header(k, v);
    }
    b.header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn s(hub: &Hub, id: &Identity, method: &str, path: &str, body: &str) -> (StatusCode, Value) {
    call(&hub.app, signed(id, method, path, body, now())).await
}

fn ids() -> (Identity, Identity, Identity) {
    (
        Identity::from_seed(&[1u8; 32]),
        Identity::from_seed(&[2u8; 32]),
        Identity::from_seed(&[3u8; 32]),
    )
}

fn frame(sender: &Identity, channel: &str) -> String {
    let ring = Keyring::genesis(&sender.x25519_public(), &DataKey::generate());
    let p = Payload::Node(json!({"name": "x"}));
    serde_json::to_string(&Envelope::seal_channel(sender, channel, None, &ring, &p, 1).unwrap())
        .unwrap()
}

#[tokio::test]
async fn public_endpoints() {
    let h = hub_with(&[]);
    let (st, v) = call(&h.app, Request::get("/health").body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["ok"], true);
    let (st, v) = call(
        &h.app,
        Request::get("/v0/info").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["contract_version"], proto::CONTRACT_VERSION);
}

#[tokio::test]
async fn auth_rejections() {
    let (a, _, stranger) = ids();
    let h = hub_with(&[(&a, &[MESH_CHANNEL])]);
    // No headers.
    let (st, _) = call(
        &h.app,
        Request::get("/v0/members").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    // Skewed timestamp.
    let (st, _) = call(&h.app, signed(&a, "GET", "/v0/members", "", now() - 1000)).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    // Signature over a different path.
    let mut r = signed(&a, "GET", "/v0/members", "", now());
    *r.uri_mut() = "/v0/frames?channel=@mesh".parse().unwrap();
    let (st, _) = call(&h.app, r).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    // Valid signature, not a member.
    let (st, _) = s(&h, &stranger, "GET", "/v0/members", "").await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    // Member.
    let (st, v) = s(&h, &a, "GET", "/v0/members", "").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn post_replay_is_refused_but_get_replay_is_fine() {
    let (a, _, _) = ids();
    let h = hub_with(&[(&a, &[MESH_CHANNEL])]);
    let body = frame(&a, MESH_CHANNEL);
    let ts = now();
    let (st, _) = call(&h.app, signed(&a, "POST", "/v0/frames", &body, ts)).await;
    assert_eq!(st, StatusCode::CREATED);
    let (st, _) = call(&h.app, signed(&a, "POST", "/v0/frames", &body, ts)).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let (st, _) = call(
        &h.app,
        signed(&a, "GET", "/v0/frames?channel=@mesh", "", ts),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = call(
        &h.app,
        signed(&a, "GET", "/v0/frames?channel=@mesh", "", ts),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test]
async fn frames_append_page_and_route_by_membership() {
    let (a, b, c) = ids();
    let h = hub_with(&[
        (&a, &[MESH_CHANNEL, "personal"]),
        (&b, &["@mesh"]),
        (&c, &["@mesh", "personal"]),
    ]);
    for i in 0..3 {
        let (st, v) = s(&h, &a, "POST", "/v0/frames", &frame(&a, "personal")).await;
        assert_eq!(st, StatusCode::CREATED, "{v}");
        assert_eq!(v["seq"], i + 1);
    }
    // Sender must be in the channel.
    let (st, _) = s(&h, &b, "POST", "/v0/frames", &frame(&b, "personal")).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    // Sender must match the auth key.
    let (st, _) = s(&h, &c, "POST", "/v0/frames", &frame(&a, "personal")).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    // A tampered frame fails verification.
    let mut tampered: Value = serde_json::from_str(&frame(&a, "personal")).unwrap();
    tampered["sent_ms"] = json!(99);
    let (st, _) = s(&h, &a, "POST", "/v0/frames", &tampered.to_string()).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // Reader must be in the channel.
    let (st, _) = s(&h, &b, "GET", "/v0/frames?channel=personal", "").await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, v) = s(
        &h,
        &c,
        "GET",
        "/v0/frames?channel=personal&after=0&limit=2",
        "",
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["frames"].as_array().unwrap().len(), 2);
    assert_eq!(v["next"], 2);
    assert_eq!(v["oldest"], 1);
    assert_eq!(v["latest"], 3);
    assert_eq!(v["frames"][0]["envelope"]["channel"], "personal");
    let (st, v) = s(&h, &c, "GET", "/v0/frames?channel=personal&after=2", "").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["frames"].as_array().unwrap().len(), 1);
    assert!(v["next"].is_null());
    // Empty channel is fine.
    let (st, v) = s(&h, &c, "GET", "/v0/frames?channel=@mesh", "").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["oldest"], 0);
}

#[tokio::test]
async fn cursor_behind_retention_is_gone() {
    let (a, _, _) = ids();
    let frames = Arc::new(MemoryFrames::new());
    let members = Arc::new(MemoryMembers::new());
    members
        .put(&Member {
            node_id: a.node_id(),
            x25519_pub: a.x25519_hex(),
            name: "a".into(),
            channels: vec!["personal".into()],
            admitted_ms: 0,
            admitted_by: "t".into(),
            role: Default::default(),
        })
        .unwrap();
    let app = app(frames.clone(), members, HubConfig::default());
    let h = Hub {
        app,
        members: Arc::new(MemoryMembers::new()),
    };
    for _ in 0..4 {
        s(&h, &a, "POST", "/v0/frames", &frame(&a, "personal")).await;
    }
    use hub::store::FrameStore;
    frames.prune("personal", 0, 1).unwrap(); // drops every frame
    let (st, v) = s(&h, &a, "GET", "/v0/frames?channel=personal&after=4", "").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["oldest"], 5);
    let (st, v) = s(&h, &a, "GET", "/v0/frames?channel=personal&after=0", "").await;
    assert_eq!(st, StatusCode::GONE, "{v}");
    assert!(v["oldest"].as_u64().unwrap() > 1);
    let oldest = v["oldest"].as_u64().unwrap();
    let (st, _) = s(
        &h,
        &a,
        "GET",
        &format!("/v0/frames?channel=personal&after={}", oldest - 1),
        "",
    )
    .await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test]
async fn enrollment_lifecycle_and_admit() {
    let (a, b, c) = ids();
    let h = hub_with(&[(&a, &[MESH_CHANNEL, "personal"])]);
    // Open a slot.
    let (st, v) = s(&h, &a, "PUT", "/v0/enroll/7kq4-m2xa", "{}").await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    assert_eq!(v["code"], "7KQ4M2XA");
    let (st, _) = s(&h, &a, "PUT", "/v0/enroll/7KQ4M2XA", "{}").await;
    assert_eq!(st, StatusCode::CONFLICT);
    // Nothing yet.
    let (st, _) = s(&h, &a, "GET", "/v0/enroll/7KQ4M2XA", "").await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    // Public fill: unknown code, bad body, then good.
    let req = json!({"node_id": b.node_id(), "x25519_pub": b.x25519_hex(), "name": "laptop", "contract": proto::CONTRACT_VERSION, "facts": "x86_64"});
    let fill = |code: &str, body: String| {
        Request::post(format!("/v0/enroll/{code}"))
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    };
    let (st, _) = call(&h.app, fill("ZZZZZZZZ", req.to_string())).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = call(
        &h.app,
        fill(
            "7KQ4M2XA",
            json!({"node_id": "zz", "x25519_pub": "zz", "name": "l", "contract": 1}).to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = call(&h.app, fill("7KQ4M2XA", req.to_string())).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _) = call(&h.app, fill("7KQ4M2XA", req.to_string())).await;
    assert_eq!(st, StatusCode::CONFLICT);
    // Only the inviter can take it; taking deletes.
    let (st, _) = s(&h, &a, "GET", "/v0/enroll/7KQ4M2XA", "").await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = s(&h, &a, "GET", "/v0/enroll/7KQ4M2XA", "").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    // B is still not a member.
    let (st, _) = s(&h, &b, "GET", "/v0/members", "").await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    // A may admit B into personal (A is in it) but not into work (A is not).
    let body = json!({"node_id": b.node_id(), "x25519_pub": b.x25519_hex(), "name": "laptop", "channels": ["work"]});
    let (st, _) = s(&h, &a, "POST", "/v0/admit", &body.to_string()).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let body = json!({"node_id": b.node_id(), "x25519_pub": b.x25519_hex(), "name": "laptop", "channels": ["personal"]});
    let (st, v) = s(&h, &a, "POST", "/v0/admit", &body.to_string()).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let ch = v["channels"].as_array().unwrap();
    assert!(ch.contains(&json!("@mesh")) && ch.contains(&json!("personal")));
    assert_eq!(v["admitted_by"], a.node_id());
    // B is a member now and can self-extend into a channel A never had.
    let body = json!({"node_id": b.node_id(), "x25519_pub": b.x25519_hex(), "name": "laptop", "channels": ["work"]});
    let (st, v) = s(&h, &b, "POST", "/v0/admit", &body.to_string()).await;
    assert_eq!(st, StatusCode::OK);
    assert!(v["channels"].as_array().unwrap().contains(&json!("work")));
    assert_eq!(
        h.members.get(&b.node_id()).unwrap().unwrap().channels.len(),
        3
    );
    // Removal: not self, then a stranger.
    let (st, _) = s(&h, &a, "DELETE", &format!("/v0/admit/{}", a.node_id()), "").await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = s(&h, &a, "DELETE", &format!("/v0/admit/{}", c.node_id()), "").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = s(&h, &a, "DELETE", &format!("/v0/admit/{}", b.node_id()), "").await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _) = s(&h, &b, "GET", "/v0/members", "").await;
    assert_eq!(st, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn public_fill_is_rate_limited() {
    let (a, b, _) = ids();
    let h = hub_with(&[(&a, &[MESH_CHANNEL])]);
    let req = json!({"node_id": b.node_id(), "x25519_pub": b.x25519_hex(), "name": "l", "contract": proto::CONTRACT_VERSION});
    let mut last = StatusCode::OK;
    for _ in 0..12 {
        let r = Request::post("/v0/enroll/AAAAAAAA")
            .header("content-type", "application/json")
            .header("x-forwarded-for", "203.0.113.9")
            .body(Body::from(req.to_string()))
            .unwrap();
        last = call(&h.app, r).await.0;
    }
    assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn events_stream_is_members_only() {
    let (a, _, _) = ids();
    let h = hub_with(&[(&a, &["personal"])]);
    // The stream never ends, so only the response head is inspected.
    let res = h
        .app
        .clone()
        .oneshot(signed(&a, "GET", "/v0/events", "", now()))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    let (st, _) = s(
        &h,
        &Identity::from_seed(&[9u8; 32]),
        "GET",
        "/v0/events",
        "",
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn oversize_frame_is_refused() {
    let (a, _, _) = ids();
    let h = hub_with(&[(&a, &[MESH_CHANNEL])]);
    let big = "x".repeat(proto::frame::MAX_FRAME_BYTES + 1);
    let (st, _) = s(&h, &a, "POST", "/v0/frames", &big).await;
    assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE);
}
