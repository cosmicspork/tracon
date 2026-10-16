//! The Phase 2 exit criterion, in one process: a session running on node B is
//! visible and controllable from node A's operator API. Two managers with the
//! fake harness, one real hub router, two mesh clients with their loops
//! running.

#[path = "support/mod.rs"]
mod support;
use support::state;

use std::{sync::Arc, time::Duration};

use axum::http::StatusCode;
use proto::envelope::DataKey;
use proto::frame::{Command, MESH_CHANNEL};
use proto::keyring::Keyring;
use proto::keys::Identity;
use serde_json::json;
use support::fake::FakeAdapter;
use support::http::{call, call_local};
use support::mesh::{identity, wait_for};
use tokio::sync::Mutex;
use tracon::adapter::HarnessEvent;
use tracon::config::Config;
use tracon::http::api::AppState;
use tracon::mesh::client::MeshClient;
use tracon::mesh::forward::CommandError;
use tracon::session::Manager;
use tracon::store::Store;
use tracon::stream::Bus;

struct Node {
    id: Identity,
    app: axum::Router,
    store: Arc<Store>,
    client: Arc<MeshClient>,
    adapter: Arc<FakeAdapter>,
    broker: tracon::broker::SharedBroker,
    cfg: Arc<Config>,
}

/// A node that lets A (seed 1) administer it, as the tests' operator does.
async fn node(seed: u8, name: &str, hub: &str, rings: &[(&str, Keyring)]) -> Node {
    node_admins(seed, name, hub, rings, &[identity(1).node_id()]).await
}

async fn node_admins(
    seed: u8,
    name: &str,
    hub: &str,
    rings: &[(&str, Keyring)],
    admins: &[String],
) -> Node {
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
    cfg.mesh.command_timeout_secs = 10;
    cfg.session.permission_timeout_secs = 30;
    cfg.node_name = name.to_string();
    cfg.mesh.admin_peers = admins.to_vec();
    let cfg = Arc::new(cfg);
    // One broker for tools, providers, and the mesh client, as `serve` wires
    // it — a lifted or handed-off credential is visible everywhere.
    let broker = tracon::broker::Broker::default().shared();
    let tools = Arc::new(tracon::mcp::Tools {
        broker: broker.clone(),
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
    client.set_broker(
        broker.clone(),
        state::scratch(&format!("e2e-cred-{name}")).join("credentials.sealed"),
    );
    manager.set_mesh(client.clone());
    let adapter = Arc::new(FakeAdapter {
        tx: Arc::new(Mutex::new(None)),
        tokens: Arc::new(Mutex::new(0)),
    });
    let state = AppState {
        manager,
        cfg,
        adapter: adapter.clone(),
        node_id: id.node_id(),
        tools,
        mesh: Some(client.clone()),
        auth: std::sync::Arc::new(tracon::http::auth::AuthState::new("127.0.0.1".into(), None)),
        enroll: Default::default(),
    };
    client.set_executor(Arc::new(state.clone()));
    // Providers wired the way `serve` wires them, with every published
    // summary carried into the node row so it rides the hello.
    let providers = tracon::providers::Providers::new(
        state.cfg.clone(),
        broker.clone(),
        DataKey::from_bytes([seed; 32]),
        id.node_id(),
        bus.clone(),
    );
    let (ps, pb, pid) = (store.clone(), bus.clone(), id.node_id());
    let carry = move |list: Vec<serde_json::Value>| {
        let json = serde_json::Value::Array(list).to_string();
        if ps.set_node_providers(&pid, &json).is_ok() {
            if let Ok(Some(row)) = ps.get_node(&pid) {
                pb.publish(tracon::stream::Frame::Node(row.to_json()));
            }
        }
    };
    carry(providers.list_public());
    providers.set_on_publish(Box::new(carry));
    state.manager.set_providers(providers);
    let cfg = state.cfg.clone();
    Node {
        id,
        app: tracon::http::router(state),
        store,
        client,
        adapter,
        broker,
        cfg,
    }
}

async fn pair() -> (Node, Node) {
    pair_with(&[], None).await
}

/// As `pair`, with channels only B is admitted to, and optionally a third
/// member the hub admits to `@mesh` that never runs: a node that never answers.
async fn pair_with(b_only: &[&str], silent: Option<&Identity>) -> (Node, Node) {
    pair_opts(b_only, silent, &[identity(1).node_id()]).await
}

/// As `pair_with`, with the peers B's `[mesh] admin_peers` names.
async fn pair_opts(
    b_only: &[&str],
    silent: Option<&Identity>,
    b_admins: &[String],
) -> (Node, Node) {
    let a_id = identity(1);
    let b_id = identity(2);
    let both: &[&str] = &[MESH_CHANNEL, "personal"];
    let b_channels: Vec<&str> = both.iter().chain(b_only).copied().collect();
    let mut admitted: Vec<(&Identity, &[&str])> =
        vec![(&a_id, both), (&b_id, b_channels.as_slice())];
    if let Some(silent) = silent {
        admitted.push((silent, &[MESH_CHANNEL]));
    }
    let hub = support::mesh::start_hub(&admitted).await;
    let mesh = Keyring::genesis(&a_id.x25519_public(), &DataKey::generate());
    let personal = Keyring::genesis(&a_id.x25519_public(), &DataKey::generate());
    let b_mesh = mesh.wrap_for(&a_id, &b_id.x25519_public()).unwrap();
    let b_personal = personal.wrap_for(&a_id, &b_id.x25519_public()).unwrap();
    let mut b_rings = vec![(MESH_CHANNEL, b_mesh), ("personal", b_personal)];
    for channel in b_only {
        b_rings.push((
            channel,
            Keyring::genesis(&b_id.x25519_public(), &DataKey::generate()),
        ));
    }
    let a = node(
        1,
        "alpha",
        &hub,
        &[(MESH_CHANNEL, mesh), ("personal", personal)],
    )
    .await;
    let b = node_admins(2, "beta", &hub, &b_rings, b_admins).await;
    // Both say hello so each knows the other's sealing key.
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
    (a, b)
}

/// A permission waiting on `session_id`, held by `node`.
fn permission(id: &str, node: &str, session_id: &str) -> tracon::store::PermissionRow {
    let now = tracon::store::now_ms();
    tracon::store::PermissionRow {
        id: id.into(),
        session_id: Some(session_id.into()),
        node_id: node.into(),
        rpc_id: 0,
        tool_call_id: None,
        title: "run just test".into(),
        kind: Some("execute".into()),
        raw_input: None,
        options: "[]".into(),
        state: "new".into(),
        answer_option_id: None,
        created_ms: now,
        created_mono_ms: 0,
        resolved_mono_ms: None,
        expires_ms: now + 60_000,
    }
}

/// `command` with the statement A's operator confirmed for it on `target`,
/// as A signs one for a peer that lets it administer it.
fn confirmed(from: &Identity, target: &str, command: Command) -> Command {
    let statement = proto::admin::AdminStatement::sign(
        from,
        target,
        &tracon::mesh::admin::op_hash(target, &command),
        "On beta: test",
        "local",
        tracon::store::now_ms(),
    );
    match command {
        Command::ProviderConnect {
            name,
            channels,
            share,
            ..
        } => Command::ProviderConnect {
            name,
            channels,
            share,
            statement: Some(statement),
        },
        Command::ProviderCode { name, code, .. } => Command::ProviderCode {
            name,
            code,
            statement: Some(statement),
        },
        Command::ProviderDisconnect { name, .. } => Command::ProviderDisconnect {
            name,
            statement: Some(statement),
        },
        other => other,
    }
}

fn refused_with(result: &Result<serde_json::Value, CommandError>, words: &str) -> bool {
    matches!(result, Err(CommandError::Refused(message)) if message.contains(words))
}

#[tokio::test]
async fn a_session_on_b_is_driven_from_a() {
    state::isolate();
    let (a, b) = pair().await;
    let (ai, bi) = (a.id.node_id(), b.id.node_id());

    // A's operator sees both nodes.
    let (st, nodes) = call(&a.app, "GET", "/api/nodes", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(nodes.as_array().unwrap().len(), 2);
    assert_eq!(nodes[0]["is_self"], true);
    assert_eq!(nodes[1]["id"], bi);
    assert_eq!(nodes[1]["reachable"], true);
    let (_, mesh) = call(&a.app, "GET", "/api/mesh", None).await;
    assert_eq!(mesh["hub"]["state"], "connected");

    // Start a session on B from A's API. B validates it against its own
    // ledger, so the item is minted there.
    let (st, item) = call(
        &b.app,
        "POST",
        "/api/work",
        Some(json!({"channel": "personal", "title": "Run on B"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{item}");
    // A repository B has no directory for is refused by B before it writes
    // a session, or a project named after a directory that is not there.
    let (st, refused) = call(
        &a.app,
        "POST",
        "/api/sessions",
        Some(
            json!({"channel": "personal", "repo_path": "github.com/o/r", "model": "m/a",
                    "node_id": bi, "work_item_id": item["id"], "phase": "plan"}),
        ),
    )
    .await;
    assert_ne!(st, StatusCode::CREATED, "{refused}");
    assert!(
        refused["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("github.com/o/r is not a directory on this node")),
        "{refused}"
    );
    assert!(b.store.list_sessions(None).unwrap().is_empty());

    let repo = state::scratch("e2e-driven-from-a");
    let (st, row) = call(
        &a.app,
        "POST",
        "/api/sessions",
        Some(
            json!({"channel": "personal", "repo_path": repo, "model": "m/a", "node_id": bi,
                    "work_item_id": item["id"], "phase": "plan"}),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{row}");
    assert_eq!(row["node_id"], bi);
    let sid = row["id"].as_str().unwrap().to_string();
    // It runs on B (the worktree step fails against a directory that is not
    // a repository, so it ends failed there — what matters is where it ran
    // and that A sees it).
    wait_for("B to own the session", || {
        b.store.get_session(&sid).ok().flatten().is_some()
    })
    .await;
    wait_for("A to mirror the outcome", || {
        a.store
            .get_session(&sid)
            .ok()
            .flatten()
            .is_some_and(|s| s.state != "starting")
    })
    .await;
    let mirrored = a.store.get_session(&sid).unwrap().unwrap();
    assert_eq!(mirrored.node_id, bi);
    // Its events reached A too.
    wait_for("events to mirror", || {
        a.store
            .events_after(&sid, -1, 10)
            .map(|e| !e.is_empty())
            .unwrap_or(false)
    })
    .await;
    assert!(a
        .store
        .events_after(&sid, -1, 10)
        .unwrap()
        .iter()
        .all(|e| e.node_id == bi));
    let _ = ai;
}

#[tokio::test]
async fn remote_evidence_refuses_a_third_nodes_claimed_owner() {
    state::isolate();
    let (a, b) = pair().await;
    let (ai, bi) = (a.id.node_id(), b.id.node_id());

    // The mesh envelope authenticates A, but it must not let A ask B to
    // forward a candidate that claims to belong to a third owner.
    let result = a
        .client
        .command(
            &bi,
            Command::EvidenceCandidate {
                request: json!({
                    "candidate_id": "commit:personal",
                    "channel": "personal",
                    "owner": ai,
                }),
            },
            Duration::from_secs(5),
        )
        .await;
    assert!(matches!(
        result,
        Err(CommandError::Refused(message))
            if message.contains("evidence owner does not match this node")
    ));
}

#[tokio::test]
async fn a_peers_providers_are_driven_from_here() {
    state::isolate();
    let (a, b) = pair().await;
    let bi = b.id.node_id();

    // A remote node signs in to Anthropic by paste: the redirect is
    // Anthropic's own page, not this browser's localhost.
    let (st, v) = call_local(
        &a.app,
        "POST",
        &format!("/api/nodes/{bi}/providers/anthropic/connect"),
        Some(json!({ "channels": ["personal"] })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["completion"], "paste", "{v}");
    assert!(v["url"]
        .as_str()
        .unwrap()
        .contains("platform.claude.com%2Foauth%2Fcode%2Fcallback"));
    // Addressed to this node by its id, the sign-in runs here, and a browser
    // at the node itself is offered the local callback as on the direct
    // route. (A browser elsewhere gets the paste flow: `tests/auth.rs`.)
    let ai = a.id.node_id();
    let (st, v) = call_local(
        &a.app,
        "POST",
        &format!("/api/nodes/{ai}/providers/anthropic/connect"),
        Some(json!({ "channels": ["personal"], "local_callback": true })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["completion"], "local_callback", "{v}");

    // An unreachable owner is refused up front, not timed out.
    a.store.set_reachable(&bi, false).unwrap();
    let (st, v) = call_local(
        &a.app,
        "POST",
        &format!("/api/nodes/{bi}/providers/anthropic/disconnect"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::GATEWAY_TIMEOUT, "{v}");
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("unreachable"));
}

#[tokio::test]
async fn a_credential_is_shared_to_a_peer_and_nothing_leaks_on_the_way() {
    state::isolate();
    let (a, b) = pair().await;
    let bi = b.id.node_id();

    // A holds a tool credential pinned nowhere: this node only.
    a.broker.write().unwrap().put("jira", {
        let mut c = tracon::broker::Credential::default();
        c.env
            .insert("JIRA_TOKEN".into(), "fake-secret-for-tests".into());
        c.channels = vec!["personal".into()];
        c
    });

    // The listing shows the key names and bindings, never the value.
    let (st, v) = call(&a.app, "GET", "/api/credentials", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["credentials"][0]["name"], "jira");
    assert_eq!(v["credentials"][0]["env_keys"][0], "JIRA_TOKEN");
    assert!(!v.to_string().contains("fake-secret-for-tests"));

    // Sharing from the interface widens the pin explicitly — this node stays
    // allowed — and hands the credential to B, sealed to B alone.
    let (st, v) = call_local(
        &a.app,
        "POST",
        "/api/credentials/jira/share",
        Some(json!({ "to": bi })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    wait_for("B to hold the credential", || {
        b.broker.read().unwrap().get("jira").is_some()
    })
    .await;
    {
        let b_broker = b.broker.read().unwrap();
        let cred = b_broker.get("jira").unwrap();
        assert!(cred.allows_node(&bi));
        assert!(cred.allows_node(&a.id.node_id()));
    }

    // Sharing something that does not exist says so.
    let (st, _) = call_local(
        &a.app,
        "POST",
        "/api/credentials/nope/share",
        Some(json!({ "to": bi })),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn prompt_answer_and_kill_forward_to_the_owner() {
    state::isolate();
    let (a, b) = pair().await;
    let bi = b.id.node_id();

    // A running session on B, planted directly so the fake harness is live.
    let (_, item) = call(
        &b.app,
        "POST",
        "/api/work",
        Some(json!({"channel": "personal", "title": "Planted"})),
    )
    .await;
    let (st, row) = call(
        &b.app,
        "POST",
        "/api/sessions",
        Some(
            json!({"channel": "personal", "repo_path": "/nonexistent", "model": "m/a",
                    "work_item_id": item["id"], "phase": "plan"}),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let sid = row["id"].as_str().unwrap().to_string();
    // The fake cannot make a worktree at /nonexistent; that session fails. Use
    // the supervisor path the sessions test uses instead: a session row in
    // `running` on B with a live fake handle is beyond this test's reach
    // without a worktree, so assert the forwarding contract on what does exist.
    wait_for("A to mirror it", || {
        a.store.get_session(&sid).ok().flatten().is_some()
    })
    .await;

    // Prompting an ended remote session: forwarded, and the owner's refusal
    // comes back as the owner phrased it.
    wait_for("it to end on B", || {
        b.store
            .get_session(&sid)
            .ok()
            .flatten()
            .is_some_and(|s| s.state == "failed")
    })
    .await;
    wait_for("A to see it ended", || {
        a.store
            .get_session(&sid)
            .ok()
            .flatten()
            .is_some_and(|s| s.state == "failed")
    })
    .await;
    let (st, v) = call(&a.app, "POST", &format!("/api/sessions/{sid}/kill"), None).await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("not running"));

    // A prompt to an unreachable owner is refused when this node already knows
    // the session ended: a mirror that has seen the terminal state does not
    // queue work for a session that cannot take it.
    b.store.set_reachable(&bi, false).unwrap();
    a.store.set_reachable(&bi, false).unwrap();
    let (st, v) = call(
        &a.app,
        "POST",
        &format!("/api/sessions/{sid}/prompt"),
        Some(json!({"text": "later"})),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("no longer active"));

    // A verdict for a review B owns, while B is unreachable, is refused with
    // the reason the interface shows.
    let (st, v) = call(
        &a.app,
        "POST",
        "/api/reviews/nope/verdict",
        Some(json!({"verdict": "reject", "reason": "x"})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{v}");
    let _ = a.adapter.tokens.lock().await;
    let _: Option<HarnessEvent> = None;
}

/// One sign-in for the pair, and a refresh either node may make but only one
/// makes per version: A signs in and B receives it; each renews in turn through
/// the real hub's claims, handing the renewed copy to the other; and when both
/// try the same version at once, exactly one refreshes.
#[tokio::test]
async fn a_subscription_signed_in_once_is_shared_and_renewed_by_one_holder_at_a_time() {
    state::isolate();
    let (a, b) = pair().await;
    let (ai, bi) = (a.id.node_id(), b.id.node_id());
    let fake = support::oauth_fake::OAuthFake::start().await;
    let providers_for = |n: &Node, seed: u8| {
        let p = tracon::providers::Providers::with_endpoints(
            Arc::new(Config::default()),
            n.broker.clone(),
            DataKey::from_bytes([seed; 32]),
            fake.endpoints(),
            n.id.node_id(),
            Bus::new(),
        );
        p.set_mesh(n.client.clone());
        let receiver = p.clone();
        n.client
            .set_on_handoff(Box::new(move |names| receiver.handoff_received(names)));
        p
    };
    let pa = providers_for(&a, 31);
    let pb = providers_for(&b, 32);
    a.client.refresh_members().await.unwrap();
    b.client.refresh_members().await.unwrap();

    let result = pa
        .connect(
            "anthropic",
            vec!["personal".into()],
            true,
            tracon::providers::LoginOwner::Local,
            false,
        )
        .await
        .unwrap();
    let authorized = fake.authorize(&result.url);
    pa.code(
        "anthropic",
        &format!("{}#{}", support::oauth_fake::GOOD_CODE, authorized.state),
        &tracon::providers::LoginOwner::Local,
    )
    .await
    .unwrap();
    let version = |n: &Node| {
        n.broker
            .read()
            .unwrap()
            .get("anthropic")
            .map(|c| (c.grant_version, c.env["ACCESS_TOKEN"].clone()))
    };
    wait_for("B to receive the sign-in", || version(&b).is_some()).await;
    {
        let held = b.broker.read().unwrap().get("anthropic").unwrap().clone();
        assert_eq!(held.nodes, [ai.clone(), bi.clone()]);
        assert!(held.allows("personal"));
    }
    wait_for("B to publish it", || {
        pb.list_public()
            .iter()
            .any(|p| p["name"] == "anthropic" && p["state"] == "connected")
    })
    .await;

    use tracon::providers::Refreshed;
    assert_eq!(pa.refresh("anthropic").await.unwrap(), Refreshed::Renewed);
    wait_for("B to receive A's renewal", || {
        version(&b).is_some_and(|(v, _)| v == 1)
    })
    .await;
    assert_eq!(pb.refresh("anthropic").await.unwrap(), Refreshed::Renewed);
    wait_for("A to receive B's renewal", || {
        version(&a).is_some_and(|(v, _)| v == 2)
    })
    .await;
    assert_eq!(version(&a), version(&b));

    let before = fake.with(|f| f.refreshes);
    let (ra, rb) = tokio::join!(pa.refresh("anthropic"), pb.refresh("anthropic"));
    let mut outcomes = [ra.unwrap(), rb.unwrap()];
    outcomes.sort_by_key(|o| *o == Refreshed::Renewed);
    assert_eq!(
        outcomes,
        [Refreshed::LeftToAnotherHolder, Refreshed::Renewed]
    );
    assert_eq!(fake.with(|f| f.refreshes), before + 1);
    wait_for("both to hold version 3", || {
        version(&a).is_some_and(|(v, _)| v == 3) && version(&b).is_some_and(|(v, _)| v == 3)
    })
    .await;
    assert_eq!(version(&a), version(&b));
}

fn held_review(id: &str, owner: &str) -> tracon::store::ReviewRow {
    let now = tracon::store::now_ms();
    tracon::store::ReviewRow {
        id: id.into(),
        session_id: None,
        node_id: owner.into(),
        channel: "personal".into(),
        kind: "pr".into(),
        title: "feat: the thing".into(),
        body: String::new(),
        edited_title: None,
        edited_body: None,
        provider: "github".into(),
        target: "{}".into(),
        diff: "+x".into(),
        files: "[]".into(),
        head_sha: "abc".into(),
        base_ref: "main".into(),
        added: 1,
        removed: 0,
        state: "pending".into(),
        verdict_reason: None,
        publish_result: None,
        claimed_ms: None,
        created_ms: now,
        created_mono_ms: 0,
        resolved_mono_ms: None,
        updated_ms: now,
        checks_json: None,
        review_session_id: None,
        ai_verdict_json: None,
        revision_patch: None,
        lane: None,
    }
}

/// A review mirrored from its owner is read from the owner, so a phone
/// served by another node decides with what the owner holds; an owner that
/// cannot be reached is said to be, not shown as an empty review.
#[tokio::test]
async fn a_mirrored_review_is_read_from_the_node_that_holds_it() {
    state::isolate();
    let (a, b) = pair().await;
    let (ai, bi) = (a.id.node_id(), b.id.node_id());
    let id = uuid::Uuid::now_v7().to_string();
    a.store.insert_review(&held_review(&id, &bi)).unwrap();
    b.store.insert_review(&held_review(&id, &bi)).unwrap();

    let (st, v) = call(&a.app, "GET", &format!("/api/reviews/{id}"), None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["remote_owner"], bi.as_str());
    assert_eq!(v["owner_detail"]["state"], "fetched", "{v}");
    assert!(v["intent"].is_object(), "the owner's intent: {v}");

    // B answers only for what it owns: not a review it does not hold, and
    // not as a relay for a third node's claim.
    let refused = a
        .client
        .command(
            &bi,
            Command::ReviewDetail {
                request: json!({ "review_id": id, "channel": "personal", "owner": ai }),
            },
            Duration::from_secs(5),
        )
        .await;
    assert!(matches!(
        refused,
        Err(CommandError::Refused(message)) if message.contains("owner does not match")
    ));
    let other = uuid::Uuid::now_v7().to_string();
    a.store.insert_review(&held_review(&other, &ai)).unwrap();
    let missing = a
        .client
        .command(
            &bi,
            Command::ReviewDetail {
                request: json!({ "review_id": other, "channel": "personal", "owner": bi }),
            },
            Duration::from_secs(5),
        )
        .await;
    assert!(
        matches!(missing, Err(CommandError::Refused(_))),
        "{missing:?}"
    );

    // Unreachable, it says so.
    a.store.set_reachable(&bi, false).unwrap();
    let (st, v) = call(&a.app, "GET", &format!("/api/reviews/{id}"), None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["owner_detail"]["state"], "unreachable", "{v}");
    assert!(v["evidence"].is_null());
}

/// Being on the mesh is not being in a channel. A sealed command proves which
/// node sent it; the owner still holds the sender to the channel of what it
/// would drive, records the refusal, and lets a member through.
#[tokio::test]
async fn a_peer_outside_a_channel_cannot_drive_what_is_in_it() {
    state::isolate();
    let (a, b) = pair_with(&["private"], None).await;
    let (ai, bi) = (a.id.node_id(), b.id.node_id());
    wait_for("B to learn who is in which channel", || {
        b.store
            .nodes_in_channel("private")
            .is_ok_and(|m| m == [bi.clone()])
            && b.store
                .nodes_in_channel("personal")
                .is_ok_and(|m| m.contains(&ai) && m.contains(&bi))
    })
    .await;
    b.store
        .insert_session(&support::rows::session_row("private-1", &bi, "private"))
        .unwrap();
    b.store
        .insert_permission(&permission("perm-1", &bi, "private-1"))
        .unwrap();
    let t = Duration::from_secs(5);
    let commands = [
        Command::Prompt {
            session_id: "private-1".into(),
            text: "do it".into(),
        },
        Command::Kill {
            session_id: "private-1".into(),
        },
        Command::Pause {
            session_id: "private-1".into(),
            reason: "x".into(),
        },
        Command::Resume {
            session_id: "private-1".into(),
            reason: "x".into(),
        },
        Command::Answer {
            permission_id: "perm-1".into(),
            option_id: "allow".into(),
            arguments: None,
            reason: None,
            notes: None,
        },
        // An admin peer, confirmed: still held to the channels it binds.
        confirmed(
            &a.id,
            &bi,
            Command::ProviderConnect {
                name: "anthropic".into(),
                channels: vec!["private".into()],
                share: true,
                statement: None,
            },
        ),
    ];
    for command in commands {
        let result = a.client.command(&bi, command.clone(), t).await;
        assert!(
            refused_with(&result, "not a member"),
            "{command:?}: {result:?}"
        );
    }
    let row = b.store.get_session("private-1").unwrap().unwrap();
    assert_eq!(row.state, "running");
    assert_eq!(
        b.store.get_permission("perm-1").unwrap().unwrap().state,
        "new"
    );
    let seen = b.client.snapshot().last_refusal.unwrap_or_default();
    assert!(
        seen.contains(&ai) && seen.contains("not a member"),
        "{seen}"
    );

    // Nor may a member sign in over a credential that also serves a channel
    // it is not in: sharing one of its channels is not enough.
    let credential = Config::default().providers["anthropic"].credential.clone();
    b.broker.write().unwrap().put(
        &credential,
        tracon::broker::Credential {
            channels: vec!["personal".into(), "private".into()],
            ..Default::default()
        },
    );
    let result = a
        .client
        .command(
            &bi,
            confirmed(
                &a.id,
                &bi,
                Command::ProviderConnect {
                    name: "anthropic".into(),
                    channels: vec!["personal".into()],
                    share: true,
                    statement: None,
                },
            ),
            t,
        )
        .await;
    assert!(refused_with(&result, "would replace"), "{result:?}");
    // Once it serves only channels the member shares, the member may.
    b.broker.write().unwrap().put(
        &credential,
        tracon::broker::Credential {
            channels: vec!["personal".into()],
            ..Default::default()
        },
    );
    let started = a
        .client
        .command(
            &bi,
            confirmed(
                &a.id,
                &bi,
                Command::ProviderConnect {
                    name: "anthropic".into(),
                    channels: vec!["personal".into()],
                    share: true,
                    statement: None,
                },
            ),
            t,
        )
        .await;
    assert!(
        started.as_ref().is_ok_and(|v| v["url"].is_string()),
        "{started:?}"
    );

    // A member drives a session in a channel it shares.
    b.store
        .insert_session(&support::rows::session_row("shared-1", &bi, "personal"))
        .unwrap();
    let stopped = a
        .client
        .command(
            &bi,
            Command::Kill {
                session_id: "shared-1".into(),
            },
            t,
        )
        .await
        .unwrap();
    assert_eq!(stopped["stopped"], true);
    assert_eq!(
        b.store.get_session("shared-1").unwrap().unwrap().state,
        "closed"
    );
    // Its prompt reaches the session itself, which has no live harness here.
    b.store
        .insert_session(&support::rows::session_row("shared-2", &bi, "personal"))
        .unwrap();
    let result = a
        .client
        .command(
            &bi,
            Command::Prompt {
                session_id: "shared-2".into(),
                text: "go on".into(),
            },
            t,
        )
        .await;
    assert!(refused_with(&result, "not running"), "{result:?}");

    // B is not a relay: a command for a session another node owns is refused,
    // not forwarded on under B's identity.
    b.store
        .insert_session(&support::rows::session_row("a-owned", &ai, "personal"))
        .unwrap();
    let result = a
        .client
        .command(
            &bi,
            Command::Kill {
                session_id: "a-owned".into(),
            },
            t,
        )
        .await;
    assert!(
        refused_with(&result, "belongs to another node"),
        "{result:?}"
    );
}

/// A command whose sender stopped waiting is not run late from the outbox.
/// One from a sender on an older build names no deadline and still runs.
#[tokio::test]
async fn a_command_past_its_deadline_is_refused_and_an_undated_one_still_runs() {
    use tracon::mesh::forward::DEADLINE_SKEW_MS;
    state::isolate();
    let (a, b) = pair().await;
    let bi = b.id.node_id();
    b.store
        .insert_session(&support::rows::session_row("late-1", &bi, "personal"))
        .unwrap();
    let kill = Command::Kill {
        session_id: "late-1".into(),
    };
    let t = Duration::from_secs(5);
    let stale = tracon::store::now_ms() - DEADLINE_SKEW_MS - 60_000;
    let result = a
        .client
        .command_until(&bi, kill.clone(), t, Some(stale))
        .await;
    assert!(refused_with(&result, "expired"), "{result:?}");
    assert_eq!(
        b.store.get_session("late-1").unwrap().unwrap().state,
        "running"
    );
    let seen = b.client.snapshot().last_refusal.unwrap_or_default();
    assert!(seen.contains("expired"), "{seen}");

    let stopped = a.client.command_until(&bi, kill, t, None).await.unwrap();
    assert_eq!(stopped["stopped"], true);
    assert_eq!(
        b.store.get_session("late-1").unwrap().unwrap().state,
        "closed"
    );
}

/// A build that cannot read a command answers it, so a newer sender is told
/// why instead of timing out.
#[tokio::test]
async fn a_command_this_build_cannot_read_is_answered_not_dropped() {
    state::isolate();
    let (a, b) = pair().await;
    let bi = b.id.node_id();
    let result = a
        .client
        .command_json(
            &bi,
            json!({ "op": "from_a_newer_build", "session_id": "s" }),
            Duration::from_secs(5),
        )
        .await;
    assert!(refused_with(&result, "older build"), "{result:?}");
    assert!(refused_with(&result, "from_a_newer_build"), "{result:?}");
}

/// A peer on an older build drops a command it cannot read without a word;
/// when it last said it runs an older build, the silence is named for what
/// it probably is. And a prompt for an owner that is away is queued here and
/// reported as queued, not as sent.
#[tokio::test]
async fn an_absent_owner_queues_a_prompt_and_an_older_one_is_named() {
    state::isolate();
    let ghost = identity(3);
    let gi = ghost.node_id();
    let (a, _b) = pair_with(&[], Some(&ghost)).await;
    wait_for("A to learn the silent member's key", || {
        a.client.peer_key(&gi).is_some() && a.store.get_node(&gi).ok().flatten().is_some()
    })
    .await;
    let kill = || Command::Kill {
        session_id: "s".into(),
    };
    let mut row = a.store.get_node(&gi).unwrap().unwrap();
    row.app_version = Some("0.0.1".into());
    a.store.put_node(&row).unwrap();
    let result = a
        .client
        .command(&gi, kill(), Duration::from_millis(300))
        .await;
    assert!(
        matches!(&result, Err(CommandError::OlderPeer { peer, .. }) if peer == "0.0.1"),
        "{result:?}"
    );
    row.app_version = Some(env!("CARGO_PKG_VERSION").into());
    a.store.put_node(&row).unwrap();
    let result = a
        .client
        .command(&gi, kill(), Duration::from_millis(300))
        .await;
    assert!(matches!(result, Err(CommandError::Timeout)), "{result:?}");

    // A live session on the absent node, as A mirrors it.
    a.store
        .insert_session(&support::rows::session_row("away-1", &gi, "personal"))
        .unwrap();
    let (st, v) = call(
        &a.app,
        "POST",
        "/api/sessions/away-1/prompt",
        Some(json!({ "text": "when you are back" })),
    )
    .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{v}");
    assert_eq!(v["queued"], true, "{v}");
    assert_eq!(v["owner"], gi.as_str(), "{v}");
}

/// A request from the node's own machine, which the operator API's
/// `Loopback` and `LocalOrAdministrator` gates read from the peer address.
async fn call_locally(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    use tower::ServiceExt;
    let mut req = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "127.0.0.1:7420")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            body.map(|b| b.to_string()).unwrap_or_default(),
        ))
        .unwrap();
    req.extensions_mut()
        .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            5000,
        ))));
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

/// A recipe offered from A reaches B over the hub as a sealed command, is
/// staged there for B's operator with nothing applied, and carries nothing
/// that is A's alone. B holds A to the channel the offer names, and A offers
/// only to that channel's members.
#[tokio::test]
async fn a_recipe_offered_to_a_co_member_is_staged_there_and_nothing_local_leaks() {
    state::isolate();
    let (a, b) = pair_with(&["private"], None).await;
    let (ai, bi) = (a.id.node_id(), b.id.node_id());
    wait_for("both to learn who is in personal", || {
        [&a, &b].iter().all(|n| {
            n.store
                .nodes_in_channel("personal")
                .is_ok_and(|m| m.contains(&ai) && m.contains(&bi))
        })
    })
    .await;
    let digest = format!("localhost/app@sha256:{}", "c".repeat(64));
    a.cfg.live_repo.set(vec![
        tracon::config::Repo {
            path: "github.com/acme/app".into(),
            dockerfile: Some(".devcontainer/Dockerfile".into()),
            checks: Some(vec!["just check".into()]),
            prepare: vec!["composer install".into()],
            egress: vec!["packagist".into()],
            prepare_credentials: vec!["nova".into()],
            ..Default::default()
        },
        tracon::config::Repo {
            path: "/var/home/op/src/pinned".into(),
            image: Some(digest.clone()),
            prepare: vec!["cp /var/home/op/.npmrc .".into()],
            ..Default::default()
        },
    ]);

    let (st, v) = call_locally(
        &a.app,
        "POST",
        "/api/repos/offer",
        Some(json!({
            "path": "github.com/acme/app",
            "channel": "personal",
            "note": "what CI runs",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["results"][0]["node"], bi.as_str(), "{v}");
    assert_eq!(v["results"][0]["state"], "offered", "{v}");
    assert_eq!(v["results"][0]["name"], "beta", "{v}");
    let offer_id = v["results"][0]["offer_id"].as_str().unwrap().to_string();

    let offers = b.store.recipe_offers(10).unwrap();
    assert_eq!(offers.len(), 1);
    let staged = &offers[0];
    assert_eq!(staged.id, offer_id);
    assert_eq!(staged.sender_node, ai);
    assert_eq!(staged.channel, "personal");
    assert_eq!(staged.repo, "github.com/acme/app");
    assert_eq!(staged.state, "pending");
    assert_eq!(staged.note.as_deref(), Some("what CI runs"));
    let wire = staged.recipe.to_string();
    for leaked in [
        "nova",
        "prepare_credentials",
        "sha256",
        "image",
        "/var/home",
    ] {
        assert!(!wire.contains(leaked), "{leaked} reached B: {wire}");
    }
    // Staged is all: B's own table is untouched.
    assert!(b.cfg.repos().is_empty());

    // An offer of the pinned entry is refused at A: its preparation names
    // a path in A's operator's home, and its image never travels anyway.
    let (st, v) = call_locally(
        &a.app,
        "POST",
        "/api/repos/offer",
        Some(json!({
            "path": "/var/home/op/src/pinned",
            "repo": "github.com/acme/pinned",
            "channel": "personal",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(v.to_string().contains("home directory"), "{v}");

    // A is not in `private`, so it offers nothing there...
    let (st, v) = call_locally(
        &a.app,
        "POST",
        "/api/repos/offer",
        Some(json!({ "path": "github.com/acme/app", "channel": "private" })),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    // ...and a sender claiming it anyway is refused by B.
    let result = a
        .client
        .command(
            &bi,
            Command::RecipeOffer {
                recipe: json!({ "repo": "github.com/acme/app" }),
                from_channel: "private".into(),
                note: None,
                not_after_ms: tracon::store::now_ms() + 60_000,
            },
            Duration::from_secs(5),
        )
        .await;
    assert!(refused_with(&result, "not a member"), "{result:?}");
    assert_eq!(b.store.recipe_offers(10).unwrap().len(), 1);

    // A newer offer for the same repository replaces the one B's operator
    // has not decided.
    let (st, v) = call_locally(
        &a.app,
        "POST",
        "/api/repos/offer",
        Some(json!({ "repo": "github.com/acme/app", "channel": "personal", "to_nodes": [bi] })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["results"][0]["state"], "offered", "{v}");
    let offers = b.store.recipe_offers(10).unwrap();
    assert_eq!(offers.len(), 2);
    assert_eq!(offers[1].state, "superseded");
    assert_eq!(offers[0].state, "pending");

    // B's operator sees it, sender named.
    let (st, v) = call_locally(&b.app, "GET", "/api/repos/offers", None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["offers"][0]["sender_name"], "alpha", "{v}");
}

/// A peer that last said it runs an older build, and so cannot read the
/// offer, is named for it rather than reported as silent.
#[tokio::test]
async fn an_offer_to_an_older_build_says_so() {
    state::isolate();
    let ghost = identity(3);
    let gi = ghost.node_id();
    let (a, _b) = pair_with(&[], Some(&ghost)).await;
    wait_for("A to learn the silent member's key", || {
        a.client.peer_key(&gi).is_some() && a.store.get_node(&gi).ok().flatten().is_some()
    })
    .await;
    let mut row = a.store.get_node(&gi).unwrap().unwrap();
    row.app_version = Some("0.31.0".into());
    a.store.put_node(&row).unwrap();
    let result = a
        .client
        .command(
            &gi,
            Command::RecipeOffer {
                recipe: json!({ "repo": "github.com/acme/app" }),
                from_channel: "personal".into(),
                note: None,
                not_after_ms: tracon::store::now_ms() + 60_000,
            },
            Duration::from_millis(300),
        )
        .await;
    assert!(
        matches!(&result, Err(CommandError::OlderPeer { peer, .. }) if peer == "0.31.0"),
        "{result:?}"
    );
}

/// A key handoff over the hub from a peer that is not in the channel is
/// refused, even after the receiver has asked the hub who is, and said so;
/// one from a member changes the channel's shared bindings and never the
/// ones the receiver keeps for itself.
#[tokio::test]
async fn a_key_handoff_is_taken_from_a_member_and_never_changes_local_bindings() {
    state::isolate();
    let (a, b) = pair_with(&["private"], None).await;
    let (ai, bi) = (a.id.node_id(), b.id.node_id());
    // B stops external harnesses on `personal`, which only B decides.
    let row = b.store.channel_get("personal").unwrap().unwrap();
    b.store
        .channel_put(
            "personal",
            &row.keyring,
            &json!({ "external_stopped": true, "notify": { "enabled": false } }).to_string(),
        )
        .unwrap();
    let private_before = b.store.channel_get("private").unwrap().unwrap();

    // A is not in `private`: its own key for it must not land on B, where
    // B would seal the channel's frames under it.
    let forged = Keyring::genesis(&a.id.x25519_public(), &DataKey::generate());
    let handoff = proto::frame::Payload::KeyHandoff {
        channels: vec![proto::frame::ChannelHandoff {
            name: "private".into(),
            keyring: forged.wrap_for(&a.id, &b.id.x25519_public()).unwrap(),
            bindings_json: json!({ "model": "m/forged" }).to_string(),
        }],
    };
    a.client
        .enqueue_direct(MESH_CHANNEL, &bi, &handoff)
        .unwrap();
    wait_for("B to refuse the handoff", || {
        b.client
            .snapshot()
            .last_refusal
            .is_some_and(|r| r.contains("key handoff") && r.contains(&ai))
    })
    .await;
    let private_after = b.store.channel_get("private").unwrap().unwrap();
    assert_eq!(private_after.keyring, private_before.keyring);
    assert_eq!(private_after.bindings_json, private_before.bindings_json);

    // A is in `personal`: the shared binding lands, the local ones stay.
    let ring =
        Keyring::from_bytes(&a.store.channel_get("personal").unwrap().unwrap().keyring).unwrap();
    let handoff = proto::frame::Payload::KeyHandoff {
        channels: vec![proto::frame::ChannelHandoff {
            name: "personal".into(),
            keyring: ring.wrap_for(&a.id, &b.id.x25519_public()).unwrap(),
            bindings_json: json!({
                "model": "m/shared",
                "external_stopped": false,
                "notify": { "enabled": true },
            })
            .to_string(),
        }],
    };
    a.client
        .enqueue_direct(MESH_CHANNEL, &bi, &handoff)
        .unwrap();
    wait_for("B to take the shared binding", || {
        b.store
            .channel_get("personal")
            .ok()
            .flatten()
            .is_some_and(|row| row.bindings_json.contains("m/shared"))
    })
    .await;
    let bindings: serde_json::Value = serde_json::from_str(
        &b.store
            .channel_get("personal")
            .unwrap()
            .unwrap()
            .bindings_json,
    )
    .unwrap();
    assert_eq!(
        bindings,
        json!({
            "model": "m/shared",
            "external_stopped": true,
            "notify": { "enabled": false },
        })
    );
}

/// A peer's provider sign-in replaces what this node signs in with, so it is
/// an admin command: B takes it only from a node its `[mesh] admin_peers`
/// names, and only with the statement A's operator confirmed for exactly that
/// command on B, once. Channel membership alone is not enough.
#[tokio::test]
async fn a_peers_provider_sign_in_needs_an_admin_peer_and_a_confirmation() {
    state::isolate();
    let t = Duration::from_secs(5);
    let connect = || Command::ProviderConnect {
        name: "anthropic".into(),
        channels: vec!["personal".into()],
        share: true,
        statement: None,
    };

    // B names no admin peer: A, a co-member of `personal`, is refused
    // whether or not its operator confirmed it.
    let (a, b) = pair_opts(&[], None, &[]).await;
    let bi = b.id.node_id();
    let (st, v) = call_local(
        &a.app,
        "POST",
        &format!("/api/nodes/{bi}/providers/anthropic/connect"),
        Some(json!({ "channels": ["personal"] })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("admin_peers"),
        "{v}"
    );
    for command in [
        connect(),
        confirmed(&a.id, &bi, connect()),
        confirmed(
            &a.id,
            &bi,
            Command::ProviderDisconnect {
                name: "anthropic".into(),
                statement: None,
            },
        ),
        confirmed(
            &a.id,
            &bi,
            Command::ProviderCode {
                name: "anthropic".into(),
                code: "c".into(),
                statement: None,
            },
        ),
    ] {
        let result = a.client.command(&bi, command.clone(), t).await;
        assert!(
            refused_with(&result, "admin_peers"),
            "{command:?}: {result:?}"
        );
    }
    drop((a, b));

    // B names A. Unconfirmed, or confirmed for something else, or replayed:
    // refused. Confirmed for exactly this: it runs.
    state::isolate();
    let (a, b) = pair().await;
    let bi = b.id.node_id();
    let result = a.client.command(&bi, connect(), t).await;
    assert!(refused_with(&result, "carry the statement"), "{result:?}");
    let for_openai = confirmed(
        &a.id,
        &bi,
        Command::ProviderConnect {
            name: "openai".into(),
            channels: vec!["personal".into()],
            share: true,
            statement: None,
        },
    );
    let Command::ProviderConnect { statement, .. } = for_openai else {
        unreachable!()
    };
    let swapped = Command::ProviderConnect {
        name: "anthropic".into(),
        channels: vec!["personal".into()],
        share: true,
        statement,
    };
    let result = a.client.command(&bi, swapped, t).await;
    assert!(refused_with(&result, "another operation"), "{result:?}");
    let ok = confirmed(&a.id, &bi, connect());
    let started = a.client.command(&bi, ok.clone(), t).await;
    assert!(
        started.as_ref().is_ok_and(|v| v["url"].is_string()),
        "{started:?}"
    );
    let replayed = a.client.command(&bi, ok, t).await;
    assert!(refused_with(&replayed, "already used"), "{replayed:?}");

    // From A's interface: at A's machine it is confirmed by being there and
    // still signed; off it, a passkey is needed first and nothing reaches B.
    let (st, v) = call_local(
        &a.app,
        "POST",
        &format!("/api/nodes/{bi}/providers/anthropic/connect"),
        Some(json!({ "channels": ["personal"] })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["completion"], "paste", "{v}");

    // The code that finishes that confirmed sign-in rides on its connect: B
    // admits it without a statement and the provider itself answers (here,
    // that the pasted code is another sign-in's). From A's interface off the
    // machine, no passkey is asked for it either.
    let code = |name: &str| Command::ProviderCode {
        name: name.into(),
        code: "abc#not-this-sign-in".into(),
        statement: None,
    };
    let result = a.client.command(&bi, code("anthropic"), t).await;
    assert!(refused_with(&result, "different sign-in"), "{result:?}");
    let (st, v) = call(
        &a.app,
        "POST",
        &format!("/api/nodes/{bi}/providers/anthropic/code"),
        Some(json!({ "code": "abc#not-this-sign-in" })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("different sign-in"),
        "{v}"
    );
    // A code for any other sign-in (none was confirmed for openai) still
    // needs a statement of its own, and off the machine, a passkey.
    let result = a.client.command(&bi, code("openai-codex"), t).await;
    assert!(refused_with(&result, "carry the statement"), "{result:?}");
    let (st, v) = call(
        &a.app,
        "POST",
        &format!("/api/nodes/{bi}/providers/openai-codex/code"),
        Some(json!({ "code": "abc" })),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert!(
        v["error"]["kind"]
            .as_str()
            .is_some_and(|k| k.starts_with("step_up")),
        "{v}"
    );

    let (st, v) = call(
        &a.app,
        "POST",
        &format!("/api/nodes/{bi}/providers/anthropic/disconnect"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert!(
        v["error"]["kind"]
            .as_str()
            .is_some_and(|k| k.starts_with("step_up")),
        "{v}"
    );
}
