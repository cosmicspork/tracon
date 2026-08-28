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
//! Delivery goes through the pager bridge, which holds the device keys and
//! seals to each paired device. The node hands it cleartext over a loopback or
//! pod-network hop and holds no notification secret of its own.

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
/// At most this many pushes in flight, so a slow bridge cannot pile up.
const MAX_IN_FLIGHT: usize = 8;

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

/// One push, before it is handed to the bridge.
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub kind: Kind,
    pub title: String,
    pub body: String,
    /// Where tapping it should land, when the node knows its own address.
    pub path: String,
    /// The replacement key. Distinct per item, so two waiting approvals do not
    /// collapse into one banner.
    pub tag: String,
}

impl Notification {
    fn payload(&self, link_origin: Option<&str>, now: i64) -> serde_json::Value {
        let mut v = json!({
            "title": self.title,
            "body": self.body,
            "source": "tracon",
            "tag": self.tag,
            "ts": now,
        });
        if let Some(origin) = link_origin {
            v["url"] = json!(format!("{}{}", origin.trim_end_matches('/'), self.path));
        }
        v
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

/// Which channels this node pushes for. Resolved per item, because the sink is
/// bound to the channel and exactly one node in the mesh delivers it.
struct Gate {
    store: Arc<Store>,
    node_id: String,
}

impl Gate {
    fn pushes(&self, channel: &str) -> bool {
        let Ok(Some(row)) = self.store.channel_get(channel) else {
            return false;
        };
        let Ok(bindings) = serde_json::from_str::<serde_json::Value>(&row.bindings_json) else {
            return false;
        };
        let notify = &bindings["notify"];
        notify["sink"].as_str() == Some("pager")
            && notify["node"].as_str() == Some(self.node_id.as_str())
    }
}

pub struct Notifier {
    store: Arc<Store>,
    cfg: Arc<Config>,
    gate: Gate,
    seen: Seen,
    pending: Vec<Notification>,
    http: reqwest::Client,
    permits: Arc<tokio::sync::Semaphore>,
}

impl Notifier {
    pub fn new(store: Arc<Store>, cfg: Arc<Config>, node_id: String) -> Self {
        Self {
            gate: Gate {
                store: store.clone(),
                node_id,
            },
            store,
            cfg,
            seen: Seen::default(),
            pending: Vec::new(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            permits: Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT)),
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

    /// Hand everything gathered to the bridge, without waiting on it.
    fn flush(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        if pending.is_empty() {
            return;
        }
        let now = now_ms();
        for n in Self::collapse(pending) {
            let body = n.payload(self.cfg.notify.link_origin.as_deref(), now);
            let url = self.cfg.notify.pager_url.clone();
            let http = self.http.clone();
            let permits = self.permits.clone();
            tokio::spawn(async move {
                // Dropped rather than queued when the bridge is slow: the queue
                // screen is the truth, and a stale buzz helps nobody.
                let Ok(_permit) = permits.try_acquire_owned() else {
                    tracing::warn!(tag = %body["tag"], "notification dropped; sink is backed up");
                    return;
                };
                if send(&http, &url, &body).await {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                if !send(&http, &url, &body).await {
                    tracing::warn!(tag = %body["tag"], "notification not delivered");
                }
            });
        }
    }
}

/// One attempt at the bridge. Anything other than a 2xx is a failure, and a
/// failure is only ever logged.
async fn send(http: &reqwest::Client, url: &str, body: &serde_json::Value) -> bool {
    match http.post(url).json(body).send().await {
        Ok(res) if res.status().is_success() => true,
        Ok(res) => {
            tracing::warn!(status = %res.status(), "notification sink refused");
            false
        }
        Err(e) => {
            tracing::warn!(error = %e, "notification sink unreachable");
            false
        }
    }
}

/// Watch the queue and push what starts waiting. Runs until shutdown.
pub async fn run(store: Arc<Store>, bus: Bus, cfg: Arc<Config>, node_id: String) {
    let mut n = Notifier::new(store, cfg, node_id);
    let mut rx = bus.subscribe();
    let shutdown = bus.shutdown_token();
    n.prime();

    let debounce = tokio::time::sleep(std::time::Duration::from_millis(DEBOUNCE_MS));
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
                .reset(tokio::time::Instant::now() + std::time::Duration::from_millis(DEBOUNCE_MS));
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
    fn a_link_is_carried_only_when_the_node_knows_its_address() {
        let n = note(Kind::Review, "tracon-review-x");
        let with = n.payload(Some("https://tracon.example/"), 5);
        assert_eq!(with["url"], "https://tracon.example/");
        assert_eq!(with["source"], "tracon");
        assert_eq!(with["ts"], 5);

        let without = n.payload(None, 5);
        assert!(
            without.get("url").is_none(),
            "no origin, no link: {without}"
        );
    }

    #[test]
    fn the_link_joins_origin_and_path_exactly_once() {
        let n = Notification {
            kind: Kind::Review,
            title: "t".into(),
            body: "b".into(),
            path: "/reviews/abc".into(),
            tag: "x".into(),
        };
        assert_eq!(
            n.payload(Some("https://t.example/"), 0)["url"],
            "https://t.example/reviews/abc"
        );
        assert_eq!(
            n.payload(Some("https://t.example"), 0)["url"],
            "https://t.example/reviews/abc"
        );
    }
}
