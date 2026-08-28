//! What reaches the phone, and what deliberately does not. A stub stands in
//! for the pager bridge and records exactly what the node handed it.

use std::sync::{Arc, Mutex};

use axum::{extract::State, routing::post, Json, Router};
use serde_json::{json, Value};

use tracon::{
    config::Config,
    store::{now_ms, PermissionRow, ReviewRow, SessionRow, Store},
    stream::{Bus, Frame},
};

/// The bridge, as far as the node is concerned: an endpoint that records.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<Value>>>);

async fn capture(State(c): State<Capture>, Json(v): Json<Value>) -> &'static str {
    c.0.lock().unwrap().push(v);
    "ok"
}

/// A capture endpoint on a real port, and the URL to reach it.
async fn bridge() -> (Capture, String) {
    let c = Capture::default();
    let app = Router::new()
        .route("/capture", post(capture))
        .with_state(c.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });
    (c, format!("http://{addr}/capture"))
}

fn store_with_channel(bindings: &str) -> Arc<Store> {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.ensure_peer_node("n1").unwrap();
    store.channel_put("personal", &[], bindings).unwrap();
    store
}

fn session(store: &Store, id: &str) {
    store
        .insert_session(&SessionRow {
            id: id.into(),
            node_id: "n1".into(),
            channel: "personal".into(),
            work_item_id: None,
            repo_path: "/r".into(),
            worktree_path: None,
            branch: "feat/thing".into(),
            harness_id: "fake".into(),
            harness_version: "1".into(),
            harness_session_id: None,
            container_name: None,
            model: "m/a".into(),
            project_id: None,
            phase: "execute".into(),
            policy_version: Some(4),
            review_id: None,
            budget_tokens: 1000,
            tokens_used: 0,
            cost_usd: None,
            context_used: None,
            context_size: None,
            state: "running".into(),
            end_reason: None,
            last_error: None,
            turn_active: 0,
            draft: None,
            draft_updated_ms: None,
            started_mono_ms: Some(0),
            ended_mono_ms: None,
            created_ms: now_ms(),
            updated_ms: now_ms(),
        })
        .unwrap();
}

fn permission(id: &str) -> PermissionRow {
    PermissionRow {
        id: id.into(),
        session_id: "s1".into(),
        node_id: "n1".into(),
        rpc_id: 1,
        tool_call_id: None,
        title: "run just check".into(),
        kind: Some("execute".into()),
        raw_input: None,
        options: "[]".into(),
        state: "new".into(),
        answer_option_id: None,
        created_ms: now_ms(),
        created_mono_ms: 0,
        resolved_mono_ms: None,
        expires_ms: now_ms() + 60_000,
    }
}

fn review(id: &str, state: &str) -> ReviewRow {
    ReviewRow {
        id: id.into(),
        session_id: "s1".into(),
        node_id: "n1".into(),
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
        added: 12,
        removed: 3,
        state: state.into(),
        verdict_reason: None,
        publish_result: None,
        claimed_ms: None,
        created_ms: now_ms(),
        created_mono_ms: 0,
        resolved_mono_ms: None,
        updated_ms: now_ms(),
        checks_json: None,
        review_session_id: None,
        ai_verdict_json: None,
        revision_patch: None,
    }
}

/// Start the notifier against a store and a bridge.
async fn notifier(store: Arc<Store>, url: &str) -> Bus {
    let mut cfg = Config::default();
    cfg.notify.pager_url = url.into();
    cfg.notify.link_origin = Some("https://tracon.example".into());
    let bus = Bus::new();
    tokio::spawn(tracon::notify::run(
        store,
        bus.clone(),
        Arc::new(cfg),
        "n1".into(),
    ));
    // Let the task subscribe and prime before any frame is published.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    bus
}

/// The debounce window plus room for the send.
async fn settle() {
    tokio::time::sleep(std::time::Duration::from_millis(2_600)).await;
}

const BOUND: &str = r#"{"notify":{"sink":"pager","node":"n1"}}"#;

#[tokio::test]
async fn an_approval_reaches_the_phone_once_and_carries_a_way_back() {
    let (cap, url) = bridge().await;
    let store = store_with_channel(BOUND);
    session(&store, "s1");
    let bus = notifier(store.clone(), &url).await;

    let p = permission("p1");
    store.insert_permission(&p).unwrap();
    bus.publish(Frame::Queue {
        waiting: vec![p.clone()],
    });
    settle().await;

    let sent = cap.0.lock().unwrap().clone();
    assert_eq!(sent.len(), 1, "one approval, one push: {sent:?}");
    assert_eq!(sent[0]["title"], "Approval — feat/thing");
    assert_eq!(sent[0]["body"], "run just check");
    assert_eq!(sent[0]["source"], "tracon");
    assert_eq!(sent[0]["url"], "https://tracon.example/sessions/s1");
    assert_eq!(sent[0]["tag"], "tracon-perm-p1");

    // The same queue republished is not news.
    bus.publish(Frame::Queue { waiting: vec![p] });
    settle().await;
    assert_eq!(
        cap.0.lock().unwrap().len(),
        1,
        "a republish must not re-page"
    );
}

/// A permission that expires and is asked again arrives with a new id, and
/// that genuinely is a new thing to answer.
#[tokio::test]
async fn a_re_ask_pages_again() {
    let (cap, url) = bridge().await;
    let store = store_with_channel(BOUND);
    session(&store, "s1");
    let bus = notifier(store.clone(), &url).await;

    let first = permission("p1");
    store.insert_permission(&first).unwrap();
    bus.publish(Frame::Queue {
        waiting: vec![first],
    });
    settle().await;

    bus.publish(Frame::Queue { waiting: vec![] });
    let second = permission("p2");
    store.insert_permission(&second).unwrap();
    bus.publish(Frame::Queue {
        waiting: vec![second],
    });
    settle().await;

    assert_eq!(cap.0.lock().unwrap().len(), 2);
}

/// Opening a review and walking away returns it to `new`. That is not a new
/// review, and paging for it would train the operator to ignore pages.
#[tokio::test]
async fn a_review_pages_when_it_arrives_and_when_it_comes_back_but_not_on_a_release() {
    let (cap, url) = bridge().await;
    let store = store_with_channel(BOUND);
    session(&store, "s1");
    let bus = notifier(store.clone(), &url).await;

    bus.publish(Frame::Reviews {
        waiting: vec![review("r1", "new")],
    });
    settle().await;
    let sent = cap.0.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["title"], "Review — feat: the thing");
    assert_eq!(sent[0]["body"], "+12 −3");
    assert_eq!(sent[0]["url"], "https://tracon.example/reviews/r1");

    // Claimed, then released without a verdict: back to `new`, still the same
    // review the operator already knows about.
    bus.publish(Frame::Reviews {
        waiting: vec![review("r1", "claimed")],
    });
    bus.publish(Frame::Reviews {
        waiting: vec![review("r1", "new")],
    });
    settle().await;
    assert_eq!(cap.0.lock().unwrap().len(), 1, "a release must not re-page");

    // Changes requested, then the agent hands it back: that is worth knowing.
    bus.publish(Frame::Reviews {
        waiting: vec![review("r1", "revising")],
    });
    bus.publish(Frame::Reviews {
        waiting: vec![review("r1", "new")],
    });
    settle().await;
    assert_eq!(cap.0.lock().unwrap().len(), 2, "a resubmission should page");
}

/// The sink is bound per channel and names one node, so a mesh does not send
/// the same push from every node that mirrors the queue.
#[tokio::test]
async fn only_the_bound_node_delivers() {
    for bindings in [
        r#"{"notify":{"sink":"pager","node":"n2"}}"#,
        r#"{"notify":{"sink":"tray","node":"n1"}}"#,
        r#"{}"#,
    ] {
        let (cap, url) = bridge().await;
        let store = store_with_channel(bindings);
        session(&store, "s1");
        let bus = notifier(store.clone(), &url).await;
        let p = permission("p1");
        store.insert_permission(&p).unwrap();
        bus.publish(Frame::Queue { waiting: vec![p] });
        settle().await;
        assert!(
            cap.0.lock().unwrap().is_empty(),
            "{bindings} should not deliver from this node"
        );
    }
}

/// A restart is not news. What was already waiting stays waiting silently.
#[tokio::test]
async fn the_standing_queue_is_not_announced_on_startup() {
    let (cap, url) = bridge().await;
    let store = store_with_channel(BOUND);
    session(&store, "s1");
    let existing = permission("p1");
    store.insert_permission(&existing).unwrap();

    // The notifier starts with the item already in the store.
    let bus = notifier(store.clone(), &url).await;
    bus.publish(Frame::Queue {
        waiting: vec![existing],
    });
    settle().await;
    assert!(
        cap.0.lock().unwrap().is_empty(),
        "a restart should not re-announce the backlog"
    );

    // Something genuinely new still gets through.
    let fresh = permission("p2");
    store.insert_permission(&fresh).unwrap();
    bus.publish(Frame::Queue {
        waiting: vec![permission("p1"), fresh],
    });
    settle().await;
    assert_eq!(cap.0.lock().unwrap().len(), 1);
}

/// Five approvals at once is one buzz, not five.
#[tokio::test]
async fn a_burst_arrives_as_a_count() {
    let (cap, url) = bridge().await;
    let store = store_with_channel(BOUND);
    session(&store, "s1");
    let bus = notifier(store.clone(), &url).await;

    let mut waiting = Vec::new();
    for i in 0..5 {
        let p = permission(&format!("p{i}"));
        store.insert_permission(&p).unwrap();
        waiting.push(p);
    }
    bus.publish(Frame::Queue { waiting });
    settle().await;

    let sent = cap.0.lock().unwrap().clone();
    assert_eq!(sent.len(), 1, "one summary: {sent:?}");
    assert_eq!(sent[0]["body"], "5 approvals waiting");
    assert_eq!(sent[0]["tag"], "tracon-queue-perm");
}

/// The bridge being down is not the node's problem to escalate.
#[tokio::test]
async fn a_dead_sink_is_survived_quietly() {
    let store = store_with_channel(BOUND);
    session(&store, "s1");
    // Nothing is listening on this port.
    let bus = notifier(store.clone(), "http://127.0.0.1:1/capture").await;
    let p = permission("p1");
    store.insert_permission(&p).unwrap();
    bus.publish(Frame::Queue { waiting: vec![p] });
    settle().await;

    // The bus still works and the notifier is still reading it: a second item
    // is still diffed, which it would not be if the task had died.
    let (cap, url) = bridge().await;
    let mut cfg = Config::default();
    cfg.notify.pager_url = url.clone();
    let p2 = permission("p2");
    store.insert_permission(&p2).unwrap();
    bus.publish(Frame::Queue {
        waiting: vec![permission("p1"), p2],
    });
    settle().await;
    // Delivery still fails (the notifier holds the dead URL), but nothing
    // panicked and the store is intact.
    assert!(cap.0.lock().unwrap().is_empty());
    assert_eq!(store.open_permissions().unwrap().len(), 2);
}

/// A queue that moves faster than this task reads it must not lose an item.
#[tokio::test]
async fn falling_behind_is_recovered_from_the_store() {
    let (cap, url) = bridge().await;
    let store = store_with_channel(BOUND);
    session(&store, "s1");
    let bus = notifier(store.clone(), &url).await;

    // Overrun the broadcast buffer (1024) with frames the notifier ignores,
    // so it is forced to lag rather than merely be busy.
    for i in 0..2_000 {
        bus.publish(Frame::Chunk {
            session_id: "s1".into(),
            message_id: None,
            kind: "agent",
            text: format!("{i}"),
        });
    }
    // The item that was raised while it was behind is only in the store.
    let p = permission("p1");
    store.insert_permission(&p).unwrap();
    bus.publish(Frame::Queue { waiting: vec![p] });
    settle().await;

    let sent = cap.0.lock().unwrap().clone();
    assert_eq!(
        sent.len(),
        1,
        "a lagged notifier should still find it: {sent:?}"
    );
    assert_eq!(sent[0]["tag"], "tracon-perm-p1");
}

/// Without a link origin the push still says what happened; it just cannot
/// say where to go.
#[tokio::test]
async fn a_node_that_does_not_know_its_address_sends_no_link() {
    let (cap, url) = bridge().await;
    let store = store_with_channel(BOUND);
    session(&store, "s1");
    let mut cfg = Config::default();
    cfg.notify.pager_url = url;
    cfg.notify.link_origin = None;
    let bus = Bus::new();
    tokio::spawn(tracon::notify::run(
        store.clone(),
        bus.clone(),
        Arc::new(cfg),
        "n1".into(),
    ));
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;

    let p = permission("p1");
    store.insert_permission(&p).unwrap();
    bus.publish(Frame::Queue { waiting: vec![p] });
    settle().await;

    let sent = cap.0.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].get("url"),
        None,
        "no origin, no link: {:?}",
        sent[0]
    );
    assert_eq!(sent[0]["title"], "Approval — feat/thing");
}

/// A permission whose session has not been mirrored yet has no channel to
/// route by. It must wait, not vanish.
#[tokio::test]
async fn an_item_whose_session_has_not_landed_is_not_lost() {
    let (cap, url) = bridge().await;
    let store = store_with_channel(BOUND);
    let bus = notifier(store.clone(), &url).await;

    // The permission arrives before the session it belongs to.
    let p = permission("p1");
    bus.publish(Frame::Queue {
        waiting: vec![p.clone()],
    });
    settle().await;
    assert!(cap.0.lock().unwrap().is_empty(), "nothing to route by yet");

    // The mirror lands the session; the next frame carries the item through.
    session(&store, "s1");
    store.insert_permission(&p).unwrap();
    bus.publish(Frame::Queue { waiting: vec![p] });
    settle().await;
    assert_eq!(
        cap.0.lock().unwrap().len(),
        1,
        "it should arrive once the session does"
    );
}

/// Both queues, one buzz each, and both kinds reach the phone.
#[tokio::test]
async fn reviews_and_promotions_are_pushed_too() {
    let (cap, url) = bridge().await;
    let store = store_with_channel(BOUND);
    session(&store, "s1");
    let bus = notifier(store.clone(), &url).await;

    bus.publish(Frame::Reviews {
        waiting: vec![review("r1", "new")],
    });
    bus.publish(Frame::Promotions {
        waiting: vec![tracon::store::PromotionRow {
            id: "pr1".into(),
            channel: "personal".into(),
            items_json: json!([{"text": "a"}]).to_string(),
            state: "open".into(),
            verdicts_json: None,
            decided_by: None,
            decided_ms: None,
            site: "n1".into(),
            hlc_ms: now_ms(),
            created_ms: now_ms(),
        }],
    });
    settle().await;

    let sent = cap.0.lock().unwrap().clone();
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(sent.iter().any(|s| s["tag"] == "tracon-review-r1"));
    let promo = sent
        .iter()
        .find(|s| s["tag"] == "tracon-promo-pr1")
        .unwrap();
    assert_eq!(promo["title"], "Memory promotions");
    assert_eq!(promo["url"], "https://tracon.example/promotions/pr1");
}
