//! Records converge through the hub, and keep working without it: a memory
//! retained while the hub is down is recalled locally at once, concurrent
//! offline edits resolve to the same winner everywhere once the hub returns,
//! and a delete travels the same way.

#[path = "support/mod.rs"]
mod support;
use support::state;

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
use support::mesh::{identity, wait_for};
use tracon::config::Config;
use tracon::corpus;
use tracon::mesh::client::MeshClient;
use tracon::mesh::HubState;
use tracon::store::{now_ms, Store};
use tracon::stream::{Bus, Frame};
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
                    role: Default::default(),
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

fn node(seed: u8, name: &str, hub: &str, rings: &[(&str, Keyring)]) -> Node {
    let id = identity(seed);
    let store = Arc::new(Store::open_in_memory().unwrap());
    store
        .put_node(&{
            let mut r = support::rows::node_row(&id.node_id(), name);
            r.harness_pinned = "1".into();
            r.harness_found = Some("1".into());
            r.models_json = None;
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
    state::isolate();
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
    state::isolate();
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
    let (html, html_changes) = a
        .store
        .write_html_document_change(
            &ai,
            "personal",
            "ref-bundle",
            "bundle",
            "index.html",
            vec![
                tracon::corpus::html::HtmlFile {
                    path: "index.html".into(),
                    bytes: b"<title>Synced bundle</title><script src=\"app.js\"></script>".to_vec(),
                },
                tracon::corpus::html::HtmlFile {
                    path: "app.js".into(),
                    bytes: b"document.body.dataset.ready='yes'".to_vec(),
                },
            ],
            None,
            true,
        )
        .unwrap();
    a.bus.publish(Frame::Changes {
        channel: "personal".into(),
        changes: html_changes,
    });
    wait_for("B to receive the HTML bundle", || {
        let Ok(Some(document)) = b.store.doc_get("personal", "ref-bundle") else {
            return false;
        };
        b.store
            .read_html_bundle(&document)
            .is_ok_and(|files| files.len() == 2)
    })
    .await;
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
            role: Default::default(),
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

    wait_for("C to hold both documents and the HTML bundle", || {
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
            && c.store
                .doc_get("personal", "ref-bundle")
                .ok()
                .flatten()
                .is_some_and(|document| c.store.read_html_bundle(&document).is_ok())
    })
    .await;
    let received = c.store.doc_get("personal", "ref-bundle").unwrap().unwrap();
    assert_eq!(received.hash, html.hash);
    let files = c.store.read_html_bundle(&received).unwrap();
    assert_eq!(files.len(), 2);
    let changes = c
        .store
        .changes_of_site_after(&ai, "personal", 0, 1000)
        .unwrap();
    let document_ix = changes
        .iter()
        .position(|change| change.table == "document" && change.id == html.id)
        .unwrap();
    let last_chunk_ix = changes
        .iter()
        .rposition(|change| change.table == "document_bundle_chunk")
        .unwrap();
    assert!(last_chunk_ix < document_ix);
}

/// A hand-driven node: no loops, so the test says exactly when a frame is sent
/// or pulled. `path` puts the database on disk, so a restart can reopen what
/// the last client left rather than starting clean.
fn quiet_node(
    seed: u8,
    hub: &str,
    rings: &[(&str, Keyring)],
    path: Option<&std::path::Path>,
) -> Node {
    let id = identity(seed);
    let store = Arc::new(match path {
        Some(p) => Store::open(p).unwrap(),
        None => Store::open_in_memory().unwrap(),
    });
    store
        .put_node(&{
            let mut r = support::rows::node_row(&id.node_id(), "n");
            r.x25519_pub = Some(id.x25519_hex());
            r
        })
        .unwrap();
    for (c, ring) in rings {
        store.channel_put(c, &ring.to_bytes(), "{}").unwrap();
        store.node_channel_add(&id.node_id(), c).unwrap();
    }
    let bus = Bus::new();
    let client = quiet_client(seed, hub, store.clone(), &bus);
    Node {
        id,
        store,
        bus,
        client,
    }
}

/// The node comes back: a new store handle over the same file and a fresh
/// client, the way a process restart does it.
fn restarted(seed: u8, hub: &str, path: &std::path::Path) -> Node {
    let store = Arc::new(Store::open(path).unwrap());
    let bus = Bus::new();
    let client = quiet_client(seed, hub, store.clone(), &bus);
    Node {
        id: identity(seed),
        store,
        bus,
        client,
    }
}

fn quiet_client(seed: u8, hub: &str, store: Arc<Store>, bus: &Bus) -> Arc<MeshClient> {
    let mut cfg = Config::default();
    cfg.mesh.hub_url = Some(hub.to_string());
    MeshClient::new(
        identity(seed),
        hub,
        store,
        bus.clone(),
        Arc::new(cfg),
        Default::default(),
    )
}

/// A node that dies in the middle of a sync comes back to the same ledger: the
/// batches it had applied are not applied twice, none of them is lost, what it
/// had queued still goes out, and its clock still sorts a new local write after
/// everything it took from the mesh.
#[tokio::test]
async fn a_restart_mid_sync_neither_duplicates_nor_loses_applied_batches() {
    state::isolate();
    let (a_id, b_id) = (identity(1), identity(2));
    let hub = Hub::start(&[&a_id, &b_id]).await;
    let mesh = Keyring::genesis(&a_id.x25519_public(), &DataKey::generate());
    let personal = Keyring::genesis(&a_id.x25519_public(), &DataKey::generate());
    let b_rings = [
        (
            MESH_CHANNEL,
            mesh.wrap_for(&a_id, &b_id.x25519_public()).unwrap(),
        ),
        (
            "personal",
            personal.wrap_for(&a_id, &b_id.x25519_public()).unwrap(),
        ),
    ];
    let a = quiet_node(
        1,
        &hub.url(),
        &[(MESH_CHANNEL, mesh), ("personal", personal)],
        None,
    );
    let path = state::scratch("sync-restart").join("node.sqlite");
    let b = quiet_node(2, &hub.url(), &b_rings, Some(&path));
    let (ai, bi) = (a.id.node_id(), b.id.node_id());

    let written = ["one", "two", "three"];
    for (i, body) in written.iter().enumerate() {
        let change = corpus::write(
            &a.store,
            &a.bus,
            &ai,
            "personal",
            "document",
            ChangeOp::Upsert,
            &format!("d{i}"),
            doc(&format!("guide-d{i}"), body),
        )
        .unwrap();
        a.client.on_frame(&Frame::Changes {
            channel: "personal".into(),
            changes: vec![change],
        });
    }
    assert_eq!(a.client.drain_once().await.unwrap(), 3);
    assert_eq!(b.client.pull_once().await.unwrap(), 3);
    assert_eq!(
        b.store
            .changes_of_site_after(&ai, "personal", 0, 100)
            .unwrap()
            .len(),
        3
    );

    // B queues a change of its own and then dies mid-sync: neither the cursor
    // it had moved nor the frame ids it had noted reached the disk, so the hub
    // replays every batch it just applied.
    let mine = corpus::write(
        &b.store,
        &b.bus,
        &bi,
        "personal",
        "document",
        ChangeOp::Upsert,
        "d4",
        doc("guide-d4", "from b"),
    )
    .unwrap();
    b.client.on_frame(&Frame::Changes {
        channel: "personal".into(),
        changes: vec![mine],
    });
    assert_eq!(b.client.snapshot().queued, 1);
    b.store.cursor_set("personal", 0).unwrap();
    b.store.seen_prune(now_ms() + 1).unwrap();
    drop(b);

    // It comes back over the same database.
    let b = restarted(2, &hub.url(), &path);
    assert_eq!(
        b.client.snapshot().queued,
        1,
        "what was queued outlives the process"
    );
    assert_eq!(
        b.client.pull_once().await.unwrap(),
        0,
        "the replayed batches change nothing"
    );
    let log = b
        .store
        .changes_of_site_after(&ai, "personal", 0, 100)
        .unwrap();
    assert_eq!(log.len(), 3, "applied once, not twice");
    for (i, body) in written.iter().enumerate() {
        let d = b
            .store
            .doc_get("personal", &format!("guide-d{i}"))
            .unwrap()
            .unwrap();
        assert_eq!(&d.body, body);
    }
    assert_eq!(
        b.store.change_log_max(&ai, "personal").unwrap(),
        a.store.change_log_max(&ai, "personal").unwrap(),
        "nothing was lost"
    );

    // What it had queued goes out on reconnect, and its own write counter and
    // clock carry on from where the crash left them.
    assert_eq!(b.client.drain_once().await.unwrap(), 1);
    assert_eq!(a.client.pull_once().await.unwrap(), 1);
    assert!(a.store.doc_get("personal", "guide-d4").unwrap().is_some());
    let next = corpus::write(
        &b.store,
        &b.bus,
        &bi,
        "personal",
        "document",
        ChangeOp::Upsert,
        "d5",
        doc("guide-d5", "after the restart"),
    )
    .unwrap();
    assert_eq!(next.site_seq, 2);
    let newest_applied = log.iter().map(|c| (c.hlc_ms, c.hlc_ctr)).max().unwrap();
    assert!(
        (next.hlc_ms, next.hlc_ctr) > newest_applied,
        "{next:?} does not sort after {newest_applied:?}"
    );
}

/// A hub outage costs latency and nothing else. Local work carries on, queued
/// state waits rather than dying — and no authority decision widens while the
/// hub cannot be asked: the same three actions decide the same way before,
/// during, and after.
#[tokio::test]
async fn a_hub_outage_preserves_local_work_and_widens_no_authority() {
    state::isolate();
    use tracon::authority::{AuthorityQuery, MERGE};
    use tracon::policy::{Policy, Verdict};
    use tracon::store::AuthorityGrantRow;

    let (mut hub, a, b) = pair().await;
    let (ai, bi) = (a.id.node_id(), b.id.node_id());
    let policy = Policy::shipped();
    let grant = |id: &str, verdict: &str, target: &str| AuthorityGrantRow {
        id: id.into(),
        action: MERGE.into(),
        verdict: verdict.into(),
        target: target.into(),
        channel: "personal".into(),
        session_id: None,
        revision: Some("abc1234".into()),
        expires_ms: None,
        revoked_ms: None,
        reason: id.into(),
        created_ms: now_ms(),
    };
    a.store
        .authority_grant_insert(&grant("allowed", "allow", "forge:project:pr:7"))
        .unwrap();
    a.store
        .authority_grant_insert(&grant("denied", "deny", "forge:project:pr:9"))
        .unwrap();
    let verdicts = || -> Vec<Verdict> {
        [
            "forge:project:pr:7",
            "forge:project:pr:8",
            "forge:project:pr:9",
        ]
        .iter()
        .map(|target| {
            tracon::authority::decide(
                &a.store,
                &policy,
                &AuthorityQuery {
                    channel: "personal",
                    session_id: "s1",
                    action: MERGE,
                    target,
                    revision: Some("abc1234"),
                    args: &json!({}),
                },
            )
            .unwrap()
            .verdict
        })
        .collect()
    };
    let before = verdicts();
    assert_eq!(
        before,
        vec![Verdict::Allow, Verdict::Ask, Verdict::Deny],
        "a granted merge, an ungranted one, and a denied one"
    );
    assert!(a.client.peer_reachable(&bi));

    // The hub goes away.
    hub.stop();
    wait_for("A to notice the hub is gone", || {
        matches!(a.client.snapshot().hub, HubState::Unreachable { .. })
    })
    .await;

    // Local work carries on and is readable at once.
    corpus::write(
        &a.store,
        &a.bus,
        &ai,
        "personal",
        "document",
        ChangeOp::Upsert,
        "d1",
        doc("guide-offline", "written with no hub"),
    )
    .unwrap();
    assert_eq!(
        a.store
            .doc_get("personal", "guide-offline")
            .unwrap()
            .unwrap()
            .body,
        "written with no hub"
    );
    wait_for("the change to wait in A's outbox", || {
        a.client.snapshot().queued >= 1
    })
    .await;

    // Nothing about the outage widens what may be done: the decisions are the
    // ones the operator left, and a peer that cannot be reached is not
    // reachable just because this node would like it to be.
    assert_eq!(
        verdicts(),
        before,
        "an outage decided something differently"
    );
    assert!(!a.client.peer_reachable(&bi));
    assert_eq!(a.store.authority_grants(false).unwrap().len(), 2);

    // The hub returns: the queued work flushes and the decisions are unmoved.
    hub.restart().await;
    wait_for("B to hold what A wrote offline", || {
        b.store
            .doc_get("personal", "guide-offline")
            .ok()
            .flatten()
            .is_some()
    })
    .await;
    wait_for("A to see B again", || a.client.peer_reachable(&bi)).await;
    assert_eq!(a.client.snapshot().queued, 0);
    assert_eq!(
        verdicts(),
        before,
        "reconnecting decided something differently"
    );
    assert_eq!(a.store.authority_grants(false).unwrap().len(), 2);
}
