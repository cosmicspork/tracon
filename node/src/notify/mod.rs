//! Pushing what waits on the operator to where the operator is.
//!
//! The queue is the durable truth; a push is a hint that the queue changed.
//! So this task never blocks a session, never fails one, and drops a push it
//! cannot deliver rather than retrying forever.
//!
//! It reads the same frames the interface does, off the bus — which is what
//! makes a peer's approval reach the phone too: mirrored state is published
//! untapped, and untapped still means subscribers see it.
//!
//! Delivery is Web Push, straight from this node to the push service of each
//! phone that subscribed here. Every node in a mesh delivers for its own
//! devices, so a phone subscribes at whichever node it reaches; a channel that
//! should stay quiet says so in its bindings.

pub mod webpush;

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use crate::{
    config::Config,
    store::{now_ms, Store},
    stream::{Bus, Frame},
};

/// How long to gather before sending, so a burst of five approvals is one
/// buzz rather than five.
const DEBOUNCE_MS: u64 = 2_000;
/// Above this many of one kind in a window, send a count instead of each item.
const SUMMARY_ABOVE: usize = 3;
/// At most this many pushes in flight, so a slow push service cannot pile up.
/// Per device now, so a burst to three phones is nine sends, not three.
const MAX_IN_FLIGHT: usize = 32;
/// A device that has failed this many times in a row without ever saying
/// "gone" is treated as gone anyway.
const MAX_FAILURES: i64 = 20;
/// How long a push service holds an undelivered push for a phone that is off.
const TTL_ITEM_SECS: u32 = 60 * 60;
const TTL_REVIEW_SECS: u32 = 24 * 60 * 60;

/// Knobs a test turns down; production takes the defaults.
#[derive(Debug, Clone)]
pub struct Options {
    pub debounce_ms: u64,
    pub retry_after_ms: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            debounce_ms: DEBOUNCE_MS,
            retry_after_ms: 30_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Permission,
    Review,
    Promotion,
}

impl Kind {
    fn tag(&self) -> &'static str {
        match self {
            Kind::Permission => "perm",
            Kind::Review => "review",
            Kind::Promotion => "promo",
        }
    }

    /// What a run of them is called when there are too many to list.
    fn plural(&self, n: usize) -> String {
        let noun = match (self, n) {
            (Kind::Permission, 1) => "approval",
            (Kind::Permission, _) => "approvals",
            (Kind::Review, 1) => "review",
            (Kind::Review, _) => "reviews",
            (Kind::Promotion, 1) => "memory batch",
            (Kind::Promotion, _) => "memory batches",
        };
        format!("{n} {noun} waiting")
    }
}

/// One push, before it is sealed for each device.
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub kind: Kind,
    pub title: String,
    pub body: String,
    /// Where tapping it should land. A path, not a URL: the service worker
    /// runs on the node's own origin and resolves it there.
    pub path: String,
    /// The replacement key. Distinct per item, so two waiting approvals do not
    /// collapse into one banner; the same on every node, so a phone reached
    /// by two of them sees one.
    pub tag: String,
}

impl Notification {
    /// What the service worker is handed once it decrypts the push.
    pub fn payload(&self, now: i64) -> serde_json::Value {
        json!({
            "title": self.title,
            "body": self.body,
            "path": self.path,
            "tag": self.tag,
            "kind": self.kind.tag(),
            "ts": now,
        })
    }

    /// An approval is worth nothing after it expires; a review keeps.
    fn ttl_secs(&self) -> u32 {
        match self.kind {
            Kind::Permission => TTL_ITEM_SECS,
            Kind::Review | Kind::Promotion => TTL_REVIEW_SECS,
        }
    }

    /// The push behind "Send a test".
    pub fn test() -> Self {
        Self {
            kind: Kind::Permission,
            title: "tracon".into(),
            body: "Notifications are on for this device.".into(),
            path: "/".into(),
            tag: "tracon-test".into(),
        }
    }
}

/// What was already waiting the last time a frame was read.
///
/// Permissions and promotions are keyed by id alone: one that expires is gone,
/// and the harness asking again produces a new id, which is a new notification.
/// Reviews carry their state, because a review returns to `new` both when the
/// operator closes the tab without deciding and when the agent resubmits — and
/// only the second of those is worth a buzz.
#[derive(Default)]
struct Seen {
    permissions: HashSet<String>,
    reviews: HashMap<String, String>,
    promotions: HashSet<String>,
}

/// Which channels push at all. Resolved per item from the channel's bindings.
struct Gate {
    store: Arc<Store>,
}

/// Whether a channel with these bindings notifies.
///
/// On by default: a subscribed device is the opt-in now, and a standalone
/// node whose channel rows were never bound must not stay silent. An explicit
/// `notify.enabled` wins. The Phase 6 shape — `notify.sink` naming an external
/// bridge or the desktop tray — still reads: the bridge meant "push", the tray
/// meant "not the phone", and the `node` it named no longer matters because
/// every node delivers for its own devices.
pub fn enabled(bindings: &serde_json::Value) -> bool {
    let notify = &bindings["notify"];
    if let Some(b) = notify["enabled"].as_bool() {
        return b;
    }
    match notify["sink"].as_str() {
        // The tray was the one sink that meant "not the phone"; any other
        // named sink was a bridge that pushed.
        None => true,
        Some(sink) => sink != "tray",
    }
}

/// Bindings still in the Phase 6 shape, worth one line in the log.
pub fn legacy(bindings: &serde_json::Value) -> bool {
    let notify = &bindings["notify"];
    notify["sink"].is_string() || notify["node"].is_string()
}

impl Gate {
    fn pushes(&self, channel: &str) -> bool {
        let Ok(Some(row)) = self.store.channel_get(channel) else {
            // No row at all: a standalone node's fabricated channel. Notify.
            return true;
        };
        let Ok(bindings) = serde_json::from_str::<serde_json::Value>(&row.bindings_json) else {
            return true;
        };
        enabled(&bindings)
    }
}

pub struct Notifier {
    store: Arc<Store>,
    cfg: Arc<Config>,
    opts: Options,
    gate: Gate,
    seen: Seen,
    pending: Vec<Notification>,
    permits: Arc<tokio::sync::Semaphore>,
}

impl Notifier {
    pub fn new(store: Arc<Store>, cfg: Arc<Config>, opts: Options) -> Self {
        Self {
            gate: Gate {
                store: store.clone(),
            },
            store,
            cfg,
            opts,
            seen: Seen::default(),
            pending: Vec::new(),
            permits: Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT)),
        }
    }

    /// Say once, at startup, which channels still carry the Phase 6 binding
    /// shape and how to clear it. Silence would leave dead keys replicating
    /// forever; refusing to run would be out of proportion for a hint.
    fn warn_legacy(&self) {
        let Ok(rows) = self.store.channel_list() else {
            return;
        };
        for row in rows {
            let Ok(b) = serde_json::from_str::<serde_json::Value>(&row.bindings_json) else {
                continue;
            };
            if legacy(&b) {
                tracing::warn!(
                    channel = %row.name,
                    enabled = enabled(&b),
                    "legacy notify.sink/notify.node binding; clear it with: \
                     tracon channel bind {} notify.enabled={} notify.sink= notify.node=",
                    row.name,
                    enabled(&b)
                );
            }
        }
    }

    /// Record what is already waiting without pushing any of it.
    ///
    /// A node restarts for reasons that have nothing to do with the operator —
    /// a redeploy, a laptop waking — and re-announcing the standing queue every
    /// time teaches them to swipe pushes away, which costs more than the
    /// backlog is worth. Anything a peer raised while this node was down is
    /// still announced: it was not in the store to be primed, so it arrives as
    /// new when the mirror lands it.
    fn prime(&mut self) {
        if let Ok(rows) = self.store.open_permissions() {
            self.seen.permissions = rows.into_iter().map(|p| p.id).collect();
        }
        if let Ok(rows) = self.store.open_reviews() {
            self.seen.reviews = rows.into_iter().map(|r| (r.id, r.state)).collect();
        }
        if let Ok(rows) = self.store.open_promotions() {
            self.seen.promotions = rows.into_iter().map(|p| p.id).collect();
        }
    }

    /// Re-read the queue from the store and diff, for when the bus has run
    /// ahead of us and frames were dropped.
    fn resync(&mut self) {
        if let Ok(rows) = self.store.open_permissions() {
            self.diff_permissions(&rows);
        }
        if let Ok(rows) = self.store.open_reviews() {
            self.diff_reviews(&rows);
        }
        if let Ok(rows) = self.store.open_promotions() {
            self.diff_promotions(&rows);
        }
    }

    fn diff_permissions(&mut self, waiting: &[crate::store::PermissionRow]) {
        let mut present = HashSet::with_capacity(waiting.len());
        for p in waiting {
            present.insert(p.id.clone());
            if self.seen.permissions.contains(&p.id) {
                continue;
            }
            // The channel lives on the session, and a peer's session is
            // mirrored here. Until it lands there is nothing to route by, so
            // leave the id unseen and let the next frame carry it.
            let Ok(Some(session)) = self.store.get_session(&p.session_id) else {
                present.remove(&p.id);
                continue;
            };
            if self.gate.pushes(&session.channel) {
                self.pending.push(Notification {
                    kind: Kind::Permission,
                    title: format!("Approval — {}", session.branch),
                    body: p.title.clone(),
                    path: format!("/sessions/{}", p.session_id),
                    tag: format!("tracon-perm-{}", p.id),
                });
            }
        }
        self.seen.permissions = present;
    }

    fn diff_reviews(&mut self, waiting: &[crate::store::ReviewRow]) {
        let mut present = HashMap::with_capacity(waiting.len());
        for r in waiting {
            let was = self.seen.reviews.get(&r.id).map(String::as_str);
            present.insert(r.id.clone(), r.state.clone());
            // Worth a buzz when it first arrives, and when the agent hands it
            // back after changes were asked for. Not when the operator opened
            // it and walked away: that returns it to `new` too.
            let fresh = r.state == "new" && matches!(was, None | Some("revising"));
            if fresh && self.gate.pushes(&r.channel) {
                self.pending.push(Notification {
                    kind: Kind::Review,
                    title: format!("Review — {}", r.title),
                    body: format!("+{} −{}", r.added, r.removed),
                    path: format!("/reviews/{}", r.id),
                    tag: format!("tracon-review-{}", r.id),
                });
            }
        }
        self.seen.reviews = present;
    }

    fn diff_promotions(&mut self, waiting: &[crate::store::PromotionRow]) {
        let mut present = HashSet::with_capacity(waiting.len());
        for p in waiting {
            present.insert(p.id.clone());
            if self.seen.promotions.contains(&p.id) {
                continue;
            }
            if self.gate.pushes(&p.channel) {
                self.pending.push(Notification {
                    kind: Kind::Promotion,
                    title: "Memory promotions".into(),
                    body: format!("on {}", p.channel),
                    path: format!("/promotions/{}", p.id),
                    tag: format!("tracon-promo-{}", p.id),
                });
            }
        }
        self.seen.promotions = present;
    }

    /// Collapse a burst: past a handful of one kind, a count says more than
    /// the items would, and one banner beats five.
    fn collapse(pending: Vec<Notification>) -> Vec<Notification> {
        let mut by_kind: HashMap<Kind, Vec<Notification>> = HashMap::new();
        for n in pending {
            by_kind.entry(n.kind).or_default().push(n);
        }
        let mut out = Vec::new();
        let mut kinds: Vec<_> = by_kind.into_iter().collect();
        // Stable order so a burst reads the same way twice.
        kinds.sort_by_key(|(k, _)| k.tag());
        for (kind, group) in kinds {
            if group.len() > SUMMARY_ABOVE {
                out.push(Notification {
                    kind,
                    title: "tracon".into(),
                    body: kind.plural(group.len()),
                    path: "/".into(),
                    // One summary replaces the last, unlike the items.
                    tag: format!("tracon-queue-{}", kind.tag()),
                });
            } else {
                out.extend(group);
            }
        }
        out
    }

    /// Seal everything gathered for every live device and send, without
    /// waiting on any of it.
    fn flush(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        if pending.is_empty() {
            return;
        }
        let now = now_ms();
        let devices = match self.store.push_subscriptions_live(now) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = %e, "could not list push subscriptions");
                return;
            }
        };
        if devices.is_empty() {
            return;
        }
        for n in Self::collapse(pending) {
            for device in &devices {
                let store = self.store.clone();
                let cfg = self.cfg.clone();
                let device = device.clone();
                let n = n.clone();
                let permits = self.permits.clone();
                let retry_after = self.opts.retry_after_ms;
                tokio::spawn(async move {
                    // Dropped rather than queued when the service is slow: the
                    // queue screen is the truth, and a stale buzz helps nobody.
                    let Ok(_permit) = permits.try_acquire_owned() else {
                        tracing::warn!(tag = %n.tag, "notification dropped; push is backed up");
                        return;
                    };
                    if deliver(&store, &cfg, &device, &n, now).await
                        != webpush::Outcome::Unreachable
                    {
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(retry_after)).await;
                    if deliver(&store, &cfg, &device, &n, now).await
                        == webpush::Outcome::Unreachable
                    {
                        tracing::warn!(tag = %n.tag, device = %device.id, "notification not delivered");
                    }
                });
            }
        }
    }
}

/// One push to one device, with the bookkeeping the outcome earns: a "gone"
/// forgets the device, a run of failures eventually does too, and a success
/// resets the run.
pub async fn deliver(
    store: &Arc<Store>,
    cfg: &Config,
    device: &crate::store::PushSubscriptionRow,
    n: &Notification,
    now: i64,
) -> webpush::Outcome {
    let (p256dh, auth) = match webpush::decode_keys(&device.p256dh, &device.auth) {
        Ok(k) => k,
        Err(e) => {
            tracing::warn!(device = %device.id, error = %e, "push subscription unusable; forgetting it");
            let _ = store.push_subscription_delete(&device.id);
            return webpush::Outcome::Refused(0);
        }
    };
    let sub = webpush::Subscriber {
        endpoint: &device.endpoint,
        p256dh: &p256dh,
        auth: &auth,
    };
    let body = match webpush::encrypt(&sub, n.payload(now).to_string().as_bytes()) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(device = %device.id, error = %e, "push could not be sealed");
            return webpush::Outcome::Refused(0);
        }
    };
    let vapid = webpush::Vapid::load_or_generate(store);
    let http = http_client();
    let outcome = webpush::send(
        &http,
        &vapid,
        cfg.notify.subject(),
        &sub,
        body,
        n.ttl_secs(),
        &n.tag,
    )
    .await;
    match outcome {
        webpush::Outcome::Sent => {
            let _ = store.push_subscription_ok(&device.id, now_ms());
        }
        webpush::Outcome::Gone => {
            tracing::info!(device = %device.id, "push service says the device is gone; forgetting it");
            let _ = store.push_subscription_delete(&device.id);
        }
        webpush::Outcome::Refused(status) => {
            tracing::warn!(device = %device.id, status, "push service refused the push");
        }
        webpush::Outcome::Unreachable => {
            if let Ok(n) = store.push_subscription_failed(&device.id) {
                if n >= MAX_FAILURES {
                    tracing::warn!(device = %device.id, failures = n, "device never answers; forgetting it");
                    let _ = store.push_subscription_delete(&device.id);
                }
            }
        }
    }
    outcome
}

fn http_client() -> reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_default()
        })
        .clone()
}

/// Watch the queue and push what starts waiting. Runs until shutdown.
pub async fn run(store: Arc<Store>, bus: Bus, cfg: Arc<Config>, node_id: String) {
    run_with(store, bus, cfg, node_id, Options::default()).await
}

pub async fn run_with(
    store: Arc<Store>,
    bus: Bus,
    cfg: Arc<Config>,
    _node_id: String,
    opts: Options,
) {
    let debounce_ms = opts.debounce_ms;
    let mut n = Notifier::new(store, cfg, opts);
    let mut rx = bus.subscribe();
    let shutdown = bus.shutdown_token();
    n.prime();
    n.warn_legacy();

    let debounce = tokio::time::sleep(std::time::Duration::from_millis(debounce_ms));
    tokio::pin!(debounce);
    let mut armed = false;

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = &mut debounce, if armed => {
                armed = false;
                n.flush();
            }
            frame = rx.recv() => match frame {
                Ok(Frame::Queue { waiting }) => n.diff_permissions(&waiting),
                Ok(Frame::Reviews { waiting }) => n.diff_reviews(&waiting),
                Ok(Frame::Promotions { waiting }) => n.diff_promotions(&waiting),
                Ok(_) => {}
                // The queue moved faster than this task read it. The store has
                // the answer; ask it rather than guessing what was missed.
                Err(RecvError::Lagged(n_frames)) => {
                    tracing::warn!(frames = n_frames, "notifier fell behind; resyncing from the store");
                    n.resync();
                }
                Err(RecvError::Closed) => break,
            },
        }
        if !n.pending.is_empty() && !armed {
            armed = true;
            debounce
                .as_mut()
                .reset(tokio::time::Instant::now() + std::time::Duration::from_millis(debounce_ms));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(kind: Kind, tag: &str) -> Notification {
        Notification {
            kind,
            title: "t".into(),
            body: "b".into(),
            path: "/".into(),
            tag: tag.into(),
        }
    }

    #[test]
    fn a_handful_is_listed_and_a_flood_is_counted() {
        let few = vec![
            note(Kind::Permission, "a"),
            note(Kind::Permission, "b"),
            note(Kind::Permission, "c"),
        ];
        assert_eq!(
            Notifier::collapse(few).len(),
            3,
            "three still read as three"
        );

        let many: Vec<_> = (0..7)
            .map(|i| note(Kind::Permission, &format!("p{i}")))
            .collect();
        let out = Notifier::collapse(many);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].body, "7 approvals waiting");
        assert_eq!(out[0].tag, "tracon-queue-perm");
    }

    #[test]
    fn kinds_are_collapsed_separately() {
        let mut mixed: Vec<_> = (0..5)
            .map(|i| note(Kind::Permission, &format!("p{i}")))
            .collect();
        mixed.push(note(Kind::Review, "r1"));
        let out = Notifier::collapse(mixed);
        assert_eq!(
            out.len(),
            2,
            "the flood collapses, the single review does not"
        );
        assert!(out.iter().any(|n| n.body == "5 approvals waiting"));
        assert!(out.iter().any(|n| n.tag == "r1"));
    }

    #[test]
    fn one_of_a_kind_is_named_in_the_singular() {
        assert_eq!(Kind::Review.plural(1), "1 review waiting");
        assert_eq!(Kind::Promotion.plural(2), "2 memory batches waiting");
    }

    #[test]
    fn the_payload_carries_a_path_for_the_worker_to_resolve() {
        let n = Notification {
            kind: Kind::Review,
            title: "t".into(),
            body: "b".into(),
            path: "/reviews/abc".into(),
            tag: "tracon-review-abc".into(),
        };
        let v = n.payload(5);
        assert_eq!(v["path"], "/reviews/abc");
        assert_eq!(v["tag"], "tracon-review-abc");
        assert_eq!(v["kind"], "review");
        assert_eq!(v["ts"], 5);
        assert!(v.get("url").is_none(), "no origin is baked in: {v}");
    }

    #[test]
    fn a_channel_notifies_unless_told_otherwise() {
        assert!(enabled(&json!({})));
        assert!(enabled(&json!({"notify": {"enabled": true}})));
        assert!(!enabled(&json!({"notify": {"enabled": false}})));
        // The Phase 6 shapes: a bridge sink meant push, a tray sink meant not
        // the phone, and an explicit flag beats either.
        assert!(enabled(
            &json!({"notify": {"sink": "bridge", "node": "other"}})
        ));
        assert!(!enabled(&json!({"notify": {"sink": "tray", "node": "n1"}})));
        assert!(enabled(
            &json!({"notify": {"sink": "tray", "enabled": true}})
        ));
        assert!(legacy(&json!({"notify": {"sink": "tray"}})));
        assert!(!legacy(&json!({"notify": {"enabled": true}})));
    }
}
