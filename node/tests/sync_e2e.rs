//! Records converge through the hub, and keep working without it: a memory
//! retained while the hub is down is recalled locally at once, concurrent
//! offline edits resolve to the same winner everywhere once the hub returns,
//! and a delete travels the same way.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use hub::store::{Member, MemberStore, MemoryFrames, MemoryMembers};
use hub::HubConfig;
use proto::envelope::DataKey;
use proto::frame::MESH_CHANNEL;
use proto::keyring::Keyring;
use proto::keys::Identity;
use serde_json::{json, Value};
use tracon::config::Config;
use tracon::corpus;
use tracon::mesh::client::MeshClient;
use tracon::mesh::HubState;
use tracon::store::{now_ms, NodeRow, Store};
use tracon::stream::Bus;
use tracon_sync::ChangeOp;

/// A hub that can be taken away and brought back. Aborting its accept loop
/// would not do: the nodes' keep-alive connections outlive it. Instead every
/// request is answered 503 while it is "down", which is what a pod that is
/// gone looks like to a client on the other side of a load balancer.
struct Hub {
    addr: std::net::SocketAddr,
    members: Arc<MemoryMembers>,
    down: Arc<AtomicBool>,
}

async fn gate(
    axum::extract::State(down): axum::extract::State<Arc<AtomicBool>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if down.load(Ordering::Relaxed) {
        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    next.run(req).await
}

impl Hub {
    async fn start(ids: &[&Identity]) -> Self {
        let members = Arc::new(MemoryMembers::new());
        for id in ids {
            members
                .put(&Member {
                    node_id: id.node_id(),
                    x25519_pub: id.x25519_hex(),
                    name: "n".into(),
                    channels: vec![MESH_CHANNEL.into(), "personal".into()],
                    admitted_ms: 0,
                    admitted_by: "t".into(),
                })
                .unwrap();
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let down = Arc::new(AtomicBool::new(false));
        let app = hub::app(
            Arc::new(MemoryFrames::new()),
            members.clone(),
            HubConfig::default(),
        )
        .layer(axum::middleware::from_fn_with_state(down.clone(), gate));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            addr,
            members,
            down,
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The pod goes away; its stores do not.
    fn stop(&mut self) {
        self.down.store(true, Ordering::Relaxed);
    }

    async fn restart(&mut self) {
        self.down.store(false, Ordering::Relaxed);
    }
}

struct Node {
    id: Identity,
    store: Arc<Store>,
    bus: Bus,
    client: Arc<MeshClient>,
}

fn identity(seed: u8) -> Identity {
    Identity::from_seed(&[seed; 32])
}

fn node(seed: u8, name: &str, hub: &str, rings: &[(&str, Keyring)]) -> Node {
    let id = identity(seed);
    let store = Arc::new(Store::open_in_memory().unwrap());
    store
        .put_node(&NodeRow {
            id: id.node_id(),
            name: name.into(),
            state: "ready".into(),
            failed_check: None,
            failed_detail: None,
            harness_id: "fake".into(),
            harness_pinned: "1".into(),
            harness_found: Some("1".into()),
            models_json: None,
            checked_at_ms: Some(now_ms()),
            is_self: 1,
            x25519_pub: Some(id.x25519_hex()),
            last_seen_ms: None,
            reachable: 1,
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
    cfg.mesh.heartbeat_secs = 2;
    let client = MeshClient::new(
        identity(seed),
        hub,
        store.clone(),
        bus.clone(),
        Arc::new(cfg),
        Default::default(),
    );
    bus.with_tap(client.spawn());
    Node {
        id,
        store,
        bus,
        client,
    }
}

async fn wait_for<F: Fn() -> bool>(what: &str, f: F) {
    for _ in 0..400 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

fn doc(slug: &str, body: &str) -> Value {
    json!({"channel": "personal", "slug": slug, "kind": "guide", "title": slug, "body": body,
           "hash": corpus::hash_body(body), "created_ms": now_ms(), "updated_ms": now_ms()})
}

async fn pair() -> (Hub, Node, Node) {
    let a_id = identity(1);
    let b_id = identity(2);
    let hub = Hub::start(&[&a_id, &b_id]).await;
    let mesh = Keyring::genesis(&a_id.x25519_public(), &DataKey::generate());
    let personal = Keyring::genesis(&a_id.x25519_public(), &DataKey::generate());
    let b_mesh = mesh.wrap_for(&a_id, &b_id.x25519_public()).unwrap();
    let b_personal = personal.wrap_for(&a_id, &b_id.x25519_public()).unwrap();
    let a = node(
        1,
        "alpha",
        &hub.url(),
        &[(MESH_CHANNEL, mesh), ("personal", personal)],
    );
    let b = node(
        2,
        "beta",
        &hub.url(),
        &[(MESH_CHANNEL, b_mesh), ("personal", b_personal)],
    );
    a.client.hello().await.unwrap();
    b.client.hello().await.unwrap();
    let (ai, bi) = (a.id.node_id(), b.id.node_id());
    wait_for("A to see B", || {
        a.store
            .get_node(&bi)
            .ok()
            .flatten()
            .is_some_and(|n| n.reachable == 1)
    })
    .await;
    wait_for("B to see A", || {
        b.store
            .get_node(&ai)
            .ok()
            .flatten()
            .is_some_and(|n| n.reachable == 1)
    })
    .await;
    (hub, a, b)
}

#[tokio::test]
async fn records_converge_through_the_hub_and_survive_its_absence() {
    let (mut hub, a, b) = pair().await;
    let (ai, bi) = (a.id.node_id(), b.id.node_id());

    // A document written on A reads on B.
    corpus::write(
        &a.store,
        &a.bus,
        &ai,
        "personal",
        "document",
        ChangeOp::Upsert,
        "d1",
        doc("guide-x", "from a"),
    )
    .unwrap();
    wait_for("B to hold A's document", || {
        b.store
            .doc_get("personal", "guide-x")
            .ok()
            .flatten()
            .is_some_and(|d| d.body == "from a")
    })
    .await;

    // The hub goes away.
    hub.stop();
    wait_for("A to notice the hub is gone", || {
        matches!(a.client.snapshot().hub, HubState::Unreachable { .. })
    })
    .await;

    // Work continues: a fact retained on A is recalled on A at once, locally.
    corpus::write(
        &a.store, &a.bus, &ai, "personal", "memory", ChangeOp::Upsert, "m1",
        json!({"channel": "personal", "scope": "global", "scope_ref": null, "kind": "fact",
               "body": "the test command is just test", "source_session": null, "source_node": ai,
               "confidence": 0.9, "state": "active", "created_ms": now_ms(), "updated_ms": now_ms()}),
    )
    .unwrap();
    let hits = a
        .store
        .recall("personal", "test command", None, None, None, 5)
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].kind, "fact");
    assert!(
        b.store.memory_get("m1").unwrap().is_none(),
        "B cannot see it yet"
    );
    wait_for("the change to wait in A's outbox", || {
        a.client.snapshot().queued >= 1
    })
    .await;

    // Both edit the same document offline; B's edit is the later one.
    corpus::write(
        &a.store,
        &a.bus,
        &ai,
        "personal",
        "document",
        ChangeOp::Upsert,
        "d1",
        doc("guide-x", "a offline"),
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    corpus::write(
        &b.store,
        &b.bus,
        &bi,
        "personal",
        "document",
        ChangeOp::Upsert,
        "d1",
        doc("guide-x", "b offline"),
    )
    .unwrap();

    // The hub returns; everything queued goes out and both sides agree.
    hub.restart().await;
    wait_for("B to hold A's memory", || {
        b.store.memory_get("m1").ok().flatten().is_some()
    })
    .await;
    wait_for("both to hold B's edit", || {
        let ab = a
            .store
            .doc_get("personal", "guide-x")
            .ok()
            .flatten()
            .map(|d| d.body);
        let bb = b
            .store
            .doc_get("personal", "guide-x")
            .ok()
            .flatten()
            .map(|d| d.body);
        ab.as_deref() == Some("b offline") && bb.as_deref() == Some("b offline")
    })
    .await;
    // Each site's log of the other is complete.
    assert_eq!(
        a.store.change_log_max(&bi, "personal").unwrap(),
        b.store.change_log_max(&bi, "personal").unwrap()
    );
    assert_eq!(
        b.store.change_log_max(&ai, "personal").unwrap(),
        a.store.change_log_max(&ai, "personal").unwrap()
    );
    assert!(matches!(a.client.snapshot().hub, HubState::Connected));

    // A delete on B, again across an outage, tombstones on A.
    hub.stop();
    wait_for("B to notice the hub is gone", || {
        matches!(b.client.snapshot().hub, HubState::Unreachable { .. })
    })
    .await;
    corpus::write(
        &b.store,
        &b.bus,
        &bi,
        "personal",
        "document",
        ChangeOp::Delete,
        "d1",
        Value::Null,
    )
    .unwrap();
    assert!(b.store.doc_get("personal", "guide-x").unwrap().is_none());
    hub.restart().await;
    wait_for("A to see the delete", || {
        a.store
            .doc_get("personal", "guide-x")
            .ok()
            .flatten()
            .is_none()
    })
    .await;
    let hits = a
        .store
        .recall("personal", "guide", None, None, None, 5)
        .unwrap();
    assert!(hits.iter().all(|h| h.kind != "document"), "{hits:?}");
}

#[tokio::test]
async fn a_late_joiner_backfills_records_from_each_site() {
    let (hub, a, b) = pair().await;
    let (ai, bi) = (a.id.node_id(), b.id.node_id());
    corpus::write(
        &a.store,
        &a.bus,
        &ai,
        "personal",
        "document",
        ChangeOp::Upsert,
        "d1",
        doc("guide-a", "from a"),
    )
    .unwrap();
    corpus::write(
        &b.store,
        &b.bus,
        &bi,
        "personal",
        "document",
        ChangeOp::Upsert,
        "d2",
        doc("guide-b", "from b"),
    )
    .unwrap();
    wait_for("A and B to converge", || {
        a.store
            .doc_get("personal", "guide-b")
            .ok()
            .flatten()
            .is_some()
            && b.store
                .doc_get("personal", "guide-a")
                .ok()
                .flatten()
                .is_some()
    })
    .await;

    // C is admitted and handed the keys after the writes happened. Its pull
    // starts from the hub's current tail, so what it lacks comes from the
    // sites' own logs, requested when the key arrives.
    let c_id = identity(3);
    hub.members
        .put(&Member {
            node_id: c_id.node_id(),
            x25519_pub: c_id.x25519_hex(),
            name: "gamma".into(),
            channels: vec![MESH_CHANNEL.into(), "personal".into()],
            admitted_ms: 0,
            admitted_by: "t".into(),
        })
        .unwrap();
    let mesh_ring =
        Keyring::from_bytes(&a.store.channel_get(MESH_CHANNEL).unwrap().unwrap().keyring).unwrap();
    let c_mesh = mesh_ring.wrap_for(&a.id, &c_id.x25519_public()).unwrap();
    let c = node(3, "gamma", &hub.url(), &[(MESH_CHANNEL, c_mesh)]);
    c.client.hello().await.unwrap();
    // Refresh so A learns C's sealing key before handing off.
    a.client.refresh_members().await.unwrap();
    wait_for("A to see C", || {
        a.store.get_node(&c_id.node_id()).ok().flatten().is_some()
    })
    .await;
    let handoff = tracon::mesh::enroll::handoff_payload(
        &a.store,
        &a.id,
        &c_id.x25519_public(),
        &["personal".into()],
    )
    .unwrap();
    a.client
        .enqueue_direct(MESH_CHANNEL, &c_id.node_id(), &handoff)
        .unwrap();

    wait_for("C to hold both documents", || {
        c.store
            .doc_get("personal", "guide-a")
            .ok()
            .flatten()
            .is_some()
            && c.store
                .doc_get("personal", "guide-b")
                .ok()
                .flatten()
                .is_some()
    })
    .await;
}
