//! What reaches the phone, and what deliberately does not. A fake push
//! service stands in for Apple's or Google's: it holds the device's private
//! key, so it can open what the node sealed and say exactly what was sent.

#[path = "support/mod.rs"]
mod support;
use support::state;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::post,
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use p256::SecretKey;
use serde_json::{json, Value};

use tracon::{
    config::Config,
    notify::{webpush, Options},
    store::{now_ms, PermissionRow, PushSubscriptionRow, ReviewRow, Store},
    stream::{Bus, Frame},
};

/// One push the service received, opened with the device's key.
#[derive(Debug, Clone)]
struct Delivery {
    device: String,
    payload: Value,
    ttl: u32,
    topic: String,
    authorization: String,
}

/// A device: the key pair a browser would hold, and the auth secret.
#[derive(Clone)]
struct Device {
    secret: SecretKey,
    auth: [u8; 16],
}

impl Device {
    fn new() -> Self {
        let bytes: [u8; 32] = rand::random();
        Self {
            secret: SecretKey::from_slice(&bytes).unwrap(),
            auth: rand::random(),
        }
    }
    fn p256dh(&self) -> String {
        use p256::elliptic_curve::sec1::ToEncodedPoint;
        URL_SAFE_NO_PAD.encode(self.secret.public_key().to_encoded_point(false).as_bytes())
    }
    fn auth_b64(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.auth)
    }
}

#[derive(Clone)]
struct Service {
    got: Arc<Mutex<Vec<Delivery>>>,
    wake: Arc<tokio::sync::Notify>,
    devices: Arc<Mutex<std::collections::HashMap<String, Device>>>,
    /// What to answer; the node's handling of each answer is the subject.
    status: StatusCode,
}

async fn receive(
    State(s): State<Service>,
    Path(device): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let dev = s.devices.lock().unwrap().get(&device).cloned().unwrap();
    let plain =
        webpush::decrypt(&dev.secret, &dev.auth, &body).expect("the node sealed to this device");
    let h = |k: &str| {
        headers
            .get(k)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    assert_eq!(h("content-encoding"), "aes128gcm");
    s.got.lock().unwrap().push(Delivery {
        device,
        payload: serde_json::from_slice(&plain).unwrap(),
        ttl: h("ttl").parse().unwrap(),
        topic: h("topic"),
        authorization: h("authorization"),
    });
    s.wake.notify_waiters();
    s.status
}

/// A push service on a real port answering `status`, and its base URL.
async fn push_service(status: StatusCode) -> (Service, String) {
    let s = Service {
        got: Default::default(),
        wake: Default::default(),
        devices: Default::default(),
        status,
    };
    let app = Router::new()
        .route("/push/{device}", post(receive))
        .with_state(s.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });
    // Plain http is accepted for a loopback endpoint only; a real push
    // service is https and elsewhere.
    (s, format!("http://127.0.0.1:{}/push", addr.port()))
}

impl Service {
    /// Register a device with the node, as the browser's POST would.
    fn subscribe(
        &self,
        store: &Store,
        base: &str,
        name: &str,
        session_hash: Option<&str>,
    ) -> Device {
        let dev = Device::new();
        self.devices
            .lock()
            .unwrap()
            .insert(name.into(), dev.clone());
        store
            .push_subscription_upsert(&PushSubscriptionRow {
                id: format!("dev-{name}"),
                session_hash: session_hash.map(String::from),
                endpoint: format!("{base}/{name}"),
                p256dh: dev.p256dh(),
                auth: dev.auth_b64(),
                user_agent: Some("test".into()),
                created_ms: now_ms(),
                last_ok_ms: None,
                fail_count: 0,
            })
            .unwrap();
        dev
    }

    fn sent(&self) -> Vec<Delivery> {
        self.got.lock().unwrap().clone()
    }

    /// Wait until `n` pushes have arrived, or give up after `for_ms`.
    async fn wait(&self, n: usize, for_ms: u64) -> Vec<Delivery> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(for_ms);
        loop {
            let got = self.sent();
            if got.len() >= n {
                return got;
            }
            if tokio::time::timeout_at(deadline, self.wake.notified())
                .await
                .is_err()
            {
                return self.sent();
            }
        }
    }
}

/// Room for a push that should *not* arrive to have arrived.
async fn quiet() {
    tokio::time::sleep(Duration::from_millis(200)).await;
}

fn store_with_channel(bindings: &str) -> Arc<Store> {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.ensure_peer_node("n1").unwrap();
    store.channel_put("personal", &[], bindings).unwrap();
    store
}

fn session(store: &Store, id: &str) {
    let mut r = support::rows::session_row(id, "n1", "personal");
    r.branch = "feat/thing".into();
    r.model = "m/a".into();
    r.policy_version = Some(4);
    r.started_mono_ms = Some(0);
    store.insert_session(&r).unwrap();
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

/// Start the notifier against a store, with the windows turned down.
async fn notifier(store: Arc<Store>) -> Bus {
    let mut cfg = Config::default();
    cfg.notify.contact = Some("mailto:ops@tracon.example".into());
    let bus = Bus::new();
    tokio::spawn(tracon::notify::run_with(
        store,
        bus.clone(),
        Arc::new(cfg),
        "n1".into(),
        Options {
            debounce_ms: 30,
            retry_after_ms: 50,
        },
    ));
    // Let the task subscribe and prime before any frame is published.
    tokio::time::sleep(Duration::from_millis(80)).await;
    bus
}

/// A store, one subscribed device, and a running notifier.
async fn rig(bindings: &str, status: StatusCode) -> (Service, Arc<Store>, Bus) {
    let (svc, base) = push_service(status).await;
    let store = store_with_channel(bindings);
    session(&store, "s1");
    svc.subscribe(&store, &base, "phone", None);
    let bus = notifier(store.clone()).await;
    (svc, store, bus)
}

#[tokio::test]
async fn an_approval_reaches_the_phone_once_and_carries_a_way_back() {
    state::isolate();
    let (svc, store, bus) = rig("{}", StatusCode::CREATED).await;

    let p = permission("p1");
    store.insert_permission(&p).unwrap();
    bus.publish(Frame::Queue {
        waiting: vec![p.clone()],
    });
    let sent = svc.wait(1, 2_000).await;
    assert_eq!(sent.len(), 1, "one approval, one push: {sent:?}");
    let d = &sent[0];
    assert_eq!(d.payload["title"], "Approval — feat/thing");
    assert_eq!(d.payload["body"], "run just check");
    assert_eq!(d.payload["path"], "/sessions/s1");
    assert_eq!(d.payload["tag"], "tracon-perm-p1");
    assert_eq!(d.payload["kind"], "perm");
    assert!(
        d.ttl <= 3_600,
        "an approval does not outlive its expiry: {}",
        d.ttl
    );
    assert!(!d.topic.is_empty() && d.topic.len() <= 32, "{}", d.topic);
    assert!(
        d.authorization.starts_with("vapid t="),
        "{}",
        d.authorization
    );
    let k = d.authorization.rsplit("k=").next().unwrap();
    assert_eq!(
        k,
        webpush::Vapid::load_or_generate(&store).public_key_b64url()
    );

    // The same queue republished is not news.
    bus.publish(Frame::Queue { waiting: vec![p] });
    quiet().await;
    assert_eq!(svc.sent().len(), 1, "a republish must not re-page");
}

/// A permission that expires and is asked again arrives with a new id, and
/// that genuinely is a new thing to answer.
#[tokio::test]
async fn a_re_ask_pages_again() {
    state::isolate();
    let (svc, store, bus) = rig("{}", StatusCode::CREATED).await;

    let first = permission("p1");
    store.insert_permission(&first).unwrap();
    bus.publish(Frame::Queue {
        waiting: vec![first],
    });
    svc.wait(1, 2_000).await;

    bus.publish(Frame::Queue { waiting: vec![] });
    let second = permission("p2");
    store.insert_permission(&second).unwrap();
    bus.publish(Frame::Queue {
        waiting: vec![second],
    });
    assert_eq!(svc.wait(2, 2_000).await.len(), 2);
}

/// Opening a review and walking away returns it to `new`. That is not a new
/// review, and paging for it would train the operator to ignore pages.
#[tokio::test]
async fn a_review_pages_when_it_arrives_and_when_it_comes_back_but_not_on_a_release() {
    state::isolate();
    let (svc, _store, bus) = rig("{}", StatusCode::CREATED).await;

    bus.publish(Frame::Reviews {
        waiting: vec![review("r1", "new")],
    });
    let sent = svc.wait(1, 2_000).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].payload["title"], "Review — feat: the thing");
    assert_eq!(sent[0].payload["body"], "+12 −3");
    assert_eq!(sent[0].payload["path"], "/reviews/r1");
    assert_eq!(sent[0].ttl, 24 * 3_600, "a review keeps for a day");

    // Claimed, then released without a verdict: back to `new`, still the same
    // review the operator already knows about.
    bus.publish(Frame::Reviews {
        waiting: vec![review("r1", "claimed")],
    });
    bus.publish(Frame::Reviews {
        waiting: vec![review("r1", "new")],
    });
    quiet().await;
    assert_eq!(svc.sent().len(), 1, "a release must not re-page");

    // Changes requested, then the agent hands it back: that is worth knowing.
    bus.publish(Frame::Reviews {
        waiting: vec![review("r1", "revising")],
    });
    bus.publish(Frame::Reviews {
        waiting: vec![review("r1", "new")],
    });
    assert_eq!(
        svc.wait(2, 2_000).await.len(),
        2,
        "a resubmission should page"
    );
}

/// Every node delivers for its own devices; the binding only says whether
/// the channel notifies at all. The Phase 6 shapes still read sensibly.
#[tokio::test]
async fn every_member_delivers_unless_the_channel_is_quiet() {
    state::isolate();
    for (bindings, delivers) in [
        (r#"{}"#, true),
        (r#"{"notify":{"enabled":true}}"#, true),
        (r#"{"notify":{"sink":"pager","node":"n2"}}"#, true),
        (r#"{"notify":{"enabled":false}}"#, false),
        (r#"{"notify":{"sink":"tray","node":"n1"}}"#, false),
    ] {
        let (svc, store, bus) = rig(bindings, StatusCode::CREATED).await;
        let p = permission("p1");
        store.insert_permission(&p).unwrap();
        bus.publish(Frame::Queue { waiting: vec![p] });
        let got = if delivers {
            svc.wait(1, 2_000).await
        } else {
            quiet().await;
            svc.sent()
        };
        assert_eq!(got.len(), usize::from(delivers), "{bindings}: {got:?}");
    }
}

/// Two phones, one push each; a phone that logged out hears nothing more.
#[tokio::test]
async fn each_device_gets_its_own_copy_and_a_revoked_session_takes_its_devices() {
    state::isolate();
    let (svc, base) = push_service(StatusCode::CREATED).await;
    let store = store_with_channel("{}");
    session(&store, "s1");
    store
        .auth_session_insert(&tracon::store::AuthSessionRow {
            token_hash: "h1".into(),
            created_ms: now_ms(),
            last_seen_ms: now_ms(),
            expires_ms: now_ms() + 60_000,
            user_agent: None,
        })
        .unwrap();
    svc.subscribe(&store, &base, "laptop", None);
    svc.subscribe(&store, &base, "phone", Some("h1"));
    let bus = notifier(store.clone()).await;

    let p = permission("p1");
    store.insert_permission(&p).unwrap();
    bus.publish(Frame::Queue { waiting: vec![p] });
    let sent = svc.wait(2, 2_000).await;
    let mut devices: Vec<_> = sent.iter().map(|d| d.device.as_str()).collect();
    devices.sort();
    assert_eq!(devices, ["laptop", "phone"]);

    // The phone's login is revoked: the next push reaches the laptop only.
    store.auth_session_delete("h1").unwrap();
    let p2 = permission("p2");
    store.insert_permission(&p2).unwrap();
    bus.publish(Frame::Queue {
        waiting: vec![permission("p1"), p2],
    });
    let sent = svc.wait(3, 2_000).await;
    assert_eq!(sent.len(), 3, "{sent:?}");
    assert_eq!(sent[2].device, "laptop");

    // And the token itself being revoked prunes the row; the machine's own
    // browser stays.
    store.set_operator_token(None).unwrap();
    let left: Vec<_> = store
        .push_subscriptions()
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(left, ["dev-laptop"]);
}

/// A restart is not news. What was already waiting stays waiting silently.
#[tokio::test]
async fn the_standing_queue_is_not_announced_on_startup() {
    state::isolate();
    let (svc, base) = push_service(StatusCode::CREATED).await;
    let store = store_with_channel("{}");
    session(&store, "s1");
    svc.subscribe(&store, &base, "phone", None);
    let existing = permission("p1");
    store.insert_permission(&existing).unwrap();

    // The notifier starts with the item already in the store.
    let bus = notifier(store.clone()).await;
    bus.publish(Frame::Queue {
        waiting: vec![existing],
    });
    quiet().await;
    assert!(
        svc.sent().is_empty(),
        "a restart should not re-announce the backlog"
    );

    // Something genuinely new still gets through.
    let fresh = permission("p2");
    store.insert_permission(&fresh).unwrap();
    bus.publish(Frame::Queue {
        waiting: vec![permission("p1"), fresh],
    });
    assert_eq!(svc.wait(1, 2_000).await.len(), 1);
}

/// Five approvals at once is one buzz, not five.
#[tokio::test]
async fn a_burst_arrives_as_a_count() {
    state::isolate();
    let (svc, store, bus) = rig("{}", StatusCode::CREATED).await;

    let mut waiting = Vec::new();
    for i in 0..5 {
        let p = permission(&format!("p{i}"));
        store.insert_permission(&p).unwrap();
        waiting.push(p);
    }
    bus.publish(Frame::Queue { waiting });
    let sent = svc.wait(1, 2_000).await;
    quiet().await;
    let sent = if svc.sent().len() > sent.len() {
        svc.sent()
    } else {
        sent
    };
    assert_eq!(sent.len(), 1, "one summary: {sent:?}");
    assert_eq!(sent[0].payload["body"], "5 approvals waiting");
    assert_eq!(sent[0].payload["tag"], "tracon-queue-perm");
}

/// The push service saying "gone" is the phone having unsubscribed; the node
/// forgets the device rather than pushing into the void forever.
#[tokio::test]
async fn a_gone_endpoint_is_forgotten() {
    state::isolate();
    let (svc, store, bus) = rig("{}", StatusCode::GONE).await;
    let p = permission("p1");
    store.insert_permission(&p).unwrap();
    bus.publish(Frame::Queue { waiting: vec![p] });
    svc.wait(1, 2_000).await;
    quiet().await;
    assert!(
        store.push_subscriptions().unwrap().is_empty(),
        "a 410 should remove the device"
    );
}

/// The push service being down is not the node's problem to escalate: one
/// retry, a note in the log, and the device is kept for next time.
#[tokio::test]
async fn a_dead_service_is_survived_quietly() {
    state::isolate();
    let (svc, store, bus) = rig("{}", StatusCode::SERVICE_UNAVAILABLE).await;
    let p = permission("p1");
    store.insert_permission(&p).unwrap();
    bus.publish(Frame::Queue { waiting: vec![p] });
    let sent = svc.wait(2, 2_000).await;
    assert_eq!(sent.len(), 2, "one attempt and one retry: {sent:?}");
    // The service records the request before the node has read its answer,
    // so the bookkeeping lands a moment after the delivery is seen.
    let mut rows = store.push_subscriptions().unwrap();
    for _ in 0..50 {
        if rows.first().is_some_and(|r| r.fail_count >= 2) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        rows = store.push_subscriptions().unwrap();
    }
    assert_eq!(rows.len(), 1, "a flaky service does not lose the device");
    assert_eq!(rows[0].fail_count, 2);

    // The notifier is still reading the bus: a second item is still diffed.
    let p2 = permission("p2");
    store.insert_permission(&p2).unwrap();
    bus.publish(Frame::Queue {
        waiting: vec![permission("p1"), p2],
    });
    assert_eq!(svc.wait(4, 2_000).await.len(), 4);
    assert_eq!(store.open_permissions().unwrap().len(), 2);
}

/// A queue that moves faster than this task reads it must not lose an item.
#[tokio::test]
async fn falling_behind_is_recovered_from_the_store() {
    state::isolate();
    let (svc, store, bus) = rig("{}", StatusCode::CREATED).await;

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
    let sent = svc.wait(1, 2_000).await;
    assert_eq!(
        sent.len(),
        1,
        "a lagged notifier should still find it: {sent:?}"
    );
    assert_eq!(sent[0].payload["tag"], "tracon-perm-p1");
}

/// A permission whose session has not been mirrored yet has no channel to
/// route by. It must wait, not vanish.
#[tokio::test]
async fn an_item_whose_session_has_not_landed_is_not_lost() {
    state::isolate();
    let (svc, base) = push_service(StatusCode::CREATED).await;
    let store = store_with_channel("{}");
    svc.subscribe(&store, &base, "phone", None);
    let bus = notifier(store.clone()).await;

    // The permission arrives before the session it belongs to.
    let p = permission("p1");
    bus.publish(Frame::Queue {
        waiting: vec![p.clone()],
    });
    quiet().await;
    assert!(svc.sent().is_empty(), "nothing to route by yet");

    // The mirror lands the session; the next frame carries the item through.
    session(&store, "s1");
    store.insert_permission(&p).unwrap();
    bus.publish(Frame::Queue { waiting: vec![p] });
    assert_eq!(
        svc.wait(1, 2_000).await.len(),
        1,
        "it should arrive once the session does"
    );
}

/// Both queues, one buzz each, and both kinds reach the phone.
#[tokio::test]
async fn reviews_and_promotions_are_pushed_too() {
    state::isolate();
    let (svc, _store, bus) = rig("{}", StatusCode::CREATED).await;

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
    let sent = svc.wait(2, 2_000).await;
    let mut tags: Vec<_> = sent
        .iter()
        .map(|d| d.payload["tag"].as_str().unwrap().to_string())
        .collect();
    tags.sort();
    assert_eq!(tags, ["tracon-promo-pr1", "tracon-review-r1"]);
    let promo = sent.iter().find(|d| d.payload["kind"] == "promo").unwrap();
    assert_eq!(promo.payload["title"], "Memory promotions");
    assert_eq!(promo.payload["path"], "/promotions/pr1");
}

/// Nothing subscribed, nothing sent, nothing logged as a failure.
#[tokio::test]
async fn no_devices_means_no_pushes_and_no_fuss() {
    state::isolate();
    let store = store_with_channel("{}");
    session(&store, "s1");
    let bus = notifier(store.clone()).await;
    let p = permission("p1");
    store.insert_permission(&p).unwrap();
    bus.publish(Frame::Queue { waiting: vec![p] });
    quiet().await;
    assert_eq!(store.open_permissions().unwrap().len(), 1);
}
