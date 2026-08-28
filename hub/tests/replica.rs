//! The replica half in-process: handed a channel's keys, the hub opens that
//! channel's changes into its own tables; a channel it was not handed stays
//! ciphertext and is only counted.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use hub::pokes::PokeHub;
use hub::replica::Replica;
use hub::store::{Member, MemberRole, MemberStore, MemoryFrames, MemoryMembers};
use hub::{admit_self, app_with_state, state_for, HubConfig};
use proto::auth::signed_headers;
use proto::envelope::DataKey;
use proto::frame::{Change, ChangeOp, ChannelHandoff, Envelope, Payload, MESH_CHANNEL};
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
fn now_ms() -> i64 {
    now() as i64 * 1000
}

struct Rig {
    app: Router,
    replica: Arc<Replica>,
    a: Identity,
    personal: Keyring,
    work: Keyring,
}

fn rig() -> Rig {
    let a = Identity::from_seed(&[1u8; 32]);
    let members = Arc::new(MemoryMembers::new());
    members
        .put(&Member {
            node_id: a.node_id(),
            x25519_pub: a.x25519_hex(),
            name: "a".into(),
            channels: vec![MESH_CHANNEL.into(), "personal".into(), "work".into()],
            admitted_ms: 0,
            admitted_by: "t".into(),
            role: MemberRole::Node,
        })
        .unwrap();
    let frames = Arc::new(MemoryFrames::new());
    let pokes = Arc::new(PokeHub::new());
    let hub_id = Identity::from_seed(&[9u8; 32]);
    let replica =
        Replica::in_memory(hub_id, frames.clone(), members.clone(), pokes.clone()).unwrap();
    admit_self(members.as_ref(), &replica, 0).unwrap();
    let app = app_with_state(state_for(
        frames,
        members,
        HubConfig::default(),
        pokes,
        Some(replica.clone()),
    ));
    Rig {
        app,
        replica,
        personal: Keyring::genesis(&a.x25519_public(), &DataKey::generate()),
        work: Keyring::genesis(&a.x25519_public(), &DataKey::generate()),
        a,
    }
}

async fn post(rig: &Rig, id: &Identity, path: &str, body: &str) -> (StatusCode, Value) {
    let mut b = Request::builder().method("POST").uri(path);
    for (k, v) in signed_headers(id, "POST", path, body.as_bytes(), now()) {
        b = b.header(k, v);
    }
    let req = b
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = rig.app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), 1 << 20).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn change(site: &str, seq: i64, id: &str, body: &str) -> Change {
    Change {
        table: "document".into(),
        op: ChangeOp::Upsert,
        id: id.into(),
        site: site.into(),
        site_seq: seq,
        hlc_ms: now_ms(),
        hlc_ctr: 0,
        row: json!({"channel": "personal", "slug": id, "kind": "guide", "title": id, "body": body, "hash": "h", "created_ms": 1, "updated_ms": 1}),
    }
}

#[tokio::test]
async fn the_hub_opens_what_it_was_handed_and_counts_what_it_was_not() {
    let r = rig();
    let a = &r.a;
    let hub_id = r.replica.node_id();
    let hub_pk = proto::keys::key32(&r.replica.x25519_hex())
        .map(x25519_dalek::PublicKey::from)
        .unwrap();

    // Info names the replica.
    let res = r
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v0/info")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let info: Value =
        serde_json::from_slice(&to_bytes(res.into_body(), 1 << 20).await.unwrap()).unwrap();
    assert_eq!(info["replica"], true);
    assert_eq!(info["hub_node_id"], hub_id);

    // Admit the hub into `personal` (role hub) — but not `work`.
    let (st, _) = post(&r, a, "/v0/admit", &json!({"node_id": hub_id, "x25519_pub": r.replica.x25519_hex(), "name": "hub", "channels": ["personal"], "role": "hub"}).to_string()).await;
    assert_eq!(st, StatusCode::OK);
    // A node that is not the hub cannot be admitted as one.
    let (st, _) = post(&r, a, "/v0/admit", &json!({"node_id": a.node_id(), "x25519_pub": a.x25519_hex(), "name": "a", "channels": [], "role": "hub"}).to_string()).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // Hand it the personal keyring, direct-sealed.
    let handoff = Payload::KeyHandoff {
        channels: vec![ChannelHandoff {
            name: "personal".into(),
            keyring: r.personal.wrap_for(a, &hub_pk).unwrap(),
            bindings_json: r#"{"processing":"hub"}"#.into(),
        }],
    };
    let env = Envelope::seal_direct(a, MESH_CHANNEL, &hub_id, &hub_pk, &handoff, now_ms()).unwrap();
    let (st, _) = post(&r, a, "/v0/frames", &serde_json::to_string(&env).unwrap()).await;
    assert_eq!(st, StatusCode::CREATED);

    // Changes on both channels.
    let personal = Payload::Changes {
        channel: "personal".into(),
        changes: vec![change(&a.node_id(), 1, "guide-x", "from a")],
    };
    let env =
        Envelope::seal_channel(a, "personal", None, &r.personal, &personal, now_ms()).unwrap();
    assert_eq!(
        post(&r, a, "/v0/frames", &serde_json::to_string(&env).unwrap())
            .await
            .0,
        StatusCode::CREATED
    );
    let mut w = change(&a.node_id(), 2, "guide-secret", "work only");
    w.row["channel"] = json!("work");
    let work = Payload::Changes {
        channel: "work".into(),
        changes: vec![w],
    };
    let env = Envelope::seal_channel(a, "work", None, &r.work, &work, now_ms()).unwrap();
    assert_eq!(
        post(&r, a, "/v0/frames", &serde_json::to_string(&env).unwrap())
            .await
            .0,
        StatusCode::CREATED
    );

    // The loop is not running in-process; drive it.
    r.replica.ingest_pending();
    assert_eq!(r.replica.readable_channels(), vec!["personal".to_string()]);
    assert_eq!(r.replica.bindings_of("personal")["processing"], "hub");
    let (body, hits, secret): (String, i64, i64) = r.replica.with_db(|c| {
        (
            c.query_row(
                "SELECT body FROM document WHERE id = 'guide-x'",
                [],
                |row| row.get(0),
            )
            .unwrap(),
            c.query_row(
                "SELECT count(*) FROM document_fts WHERE document_fts MATCH 'from'",
                [],
                |row| row.get(0),
            )
            .unwrap(),
            c.query_row(
                "SELECT count(*) FROM document WHERE id = 'guide-secret'",
                [],
                |row| row.get(0),
            )
            .unwrap(),
        )
    });
    assert_eq!(body, "from a");
    assert_eq!(hits, 1);
    assert_eq!(secret, 0, "the work channel is opaque to the hub");
    // The hub is not a member of `work`, so it never read that frame at all;
    // undecryptable counts frames on channels it reads but holds no key for.
    // Hand the hub membership of work (no keyring) and drive again.
    let (st, _) = post(&r, a, "/v0/admit", &json!({"node_id": hub_id, "x25519_pub": r.replica.x25519_hex(), "name": "hub", "channels": ["work"], "role": "hub"}).to_string()).await;
    assert_eq!(st, StatusCode::OK);
    r.replica.ingest_pending();
    assert_eq!(r.replica.undecryptable(), 1);
    assert_eq!(
        r.replica.with_db(|c| c
            .query_row("SELECT count(*) FROM document", [], |row| row
                .get::<_, i64>(0))
            .unwrap()),
        1
    );

    // A hub-authored record travels on the channel, sealed under its key, and
    // the hub answers a backfill request with its own rows only.
    r.replica
        .write_change("personal", "memory", ChangeOp::Upsert, "m-hub", json!({"channel": "personal", "scope": "global", "kind": "fact", "body": "written by the hub", "confidence": 1.0, "state": "active", "created_ms": 1, "updated_ms": 1}))
        .unwrap();
    let ask = Payload::ChangesRequest {
        channel: "personal".into(),
        after_site_seq: 0,
    };
    let env = Envelope::seal_direct(a, MESH_CHANNEL, &hub_id, &hub_pk, &ask, now_ms()).unwrap();
    assert_eq!(
        post(&r, a, "/v0/frames", &serde_json::to_string(&env).unwrap())
            .await
            .0,
        StatusCode::CREATED
    );
    r.replica.ingest_pending();
    // Read what A would pull on @mesh and personal: a batch to A and a change frame.
    let mut saw_batch = false;
    let mut saw_hub_change = false;
    for ch in [MESH_CHANNEL, "personal"] {
        let path = format!("/v0/frames?channel={ch}&after=0&limit=100");
        let mut b = Request::builder().method("GET").uri(&path);
        for (k, v) in signed_headers(a, "GET", &path, b"", now()) {
            b = b.header(k, v);
        }
        let res = r
            .app
            .clone()
            .oneshot(b.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let page: Value =
            serde_json::from_slice(&to_bytes(res.into_body(), 1 << 20).await.unwrap()).unwrap();
        for f in page["frames"].as_array().unwrap() {
            let env: Envelope = serde_json::from_value(f["envelope"].clone()).unwrap();
            if hex::encode(env.verify().unwrap()) != hub_id {
                continue;
            }
            if env.is_direct() {
                if let Ok(Payload::ChangesBatch { changes, done, .. }) = env.open_direct(a) {
                    assert!(done);
                    assert_eq!(changes.len(), 1);
                    assert_eq!(changes[0].id, "m-hub");
                    assert_eq!(changes[0].site, hub_id);
                    saw_batch = true;
                }
            } else if let Ok(Payload::Changes { changes, .. }) = env.open_channel(&r.personal, a) {
                assert_eq!(changes[0].id, "m-hub");
                saw_hub_change = true;
            }
        }
    }
    assert!(saw_batch && saw_hub_change);
}
