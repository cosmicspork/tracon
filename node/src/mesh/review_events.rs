//! Reviews on the mesh as events, and peers watching one.
//!
//! An owner tells a review's channel what changed about its reviews in
//! metadata alone ([`ReviewMeta`]): opened, revised, claimed or unclaimed,
//! closed. A review's diff, description and what only the owner holds
//! (reviewer, checks, staleness, publication) never ride those events: a peer
//! whose operator opens the review watches it ([`Watches`]), reads the whole
//! detail from the owner once, and is pushed what changes while it watches.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

use proto::frame::ReviewEvent;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::store::{reports, ReviewRow};

/// How long a watch holds without being renewed. The review page renews well
/// inside it while open.
pub const WATCH_TTL_MS: i64 = 2 * 60_000;

/// The review states still in the operator's queue.
pub const OPEN_STATES: &[&str] = &["new", "claimed", "revising", "publishing"];

/// What a review's channel is told of it: what a listing shows, never the
/// diff or the description (a report's description excepted: a report is
/// nothing but its text and has no detail held on its node).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReviewMeta {
    pub id: String,
    #[serde(default)]
    pub session_id: Option<String>,
    pub node_id: String,
    pub channel: String,
    pub kind: String,
    pub title: String,
    pub state: String,
    pub provider: String,
    pub target: String,
    pub base_ref: String,
    pub head_sha: String,
    pub added: i64,
    pub removed: i64,
    /// The changed files, as the review row holds them.
    pub files: String,
    #[serde(default)]
    pub lane: Option<String>,
    #[serde(default)]
    pub publication_failed: Option<String>,
    /// A report's acknowledgement note, or why a review went back.
    #[serde(default)]
    pub verdict_reason: Option<String>,
    #[serde(default)]
    pub claimed_ms: Option<i64>,
    /// The peers watching it now.
    #[serde(default)]
    pub claimers: Vec<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
    /// A report's text; none for a code review.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

impl ReviewMeta {
    pub fn of(r: &ReviewRow, claimers: Vec<String>) -> Self {
        Self {
            id: r.id.clone(),
            session_id: r.session_id.clone(),
            node_id: r.node_id.clone(),
            channel: r.channel.clone(),
            kind: r.kind.clone(),
            title: r.title.clone(),
            state: r.state.clone(),
            provider: r.provider.clone(),
            target: r.target.clone(),
            base_ref: r.base_ref.clone(),
            head_sha: r.head_sha.clone(),
            added: r.added,
            removed: r.removed,
            files: r.files.clone(),
            lane: r.lane.clone(),
            publication_failed: r.publication_failed.clone(),
            verdict_reason: r.verdict_reason.clone(),
            claimed_ms: r.claimed_ms,
            claimers,
            created_ms: r.created_ms,
            updated_ms: r.updated_ms,
            body: (r.kind == reports::KIND).then(|| r.body.clone()),
        }
    }

    /// Whether what a listing shows differs, a claim aside.
    fn listing_differs(&self, other: &Self) -> bool {
        let strip = |m: &Self| Self {
            claimed_ms: None,
            claimers: Vec::new(),
            updated_ms: 0,
            // A claim moves a review between `new` and `claimed`; that is a
            // claim, not a revision.
            state: if m.state == "claimed" {
                "new".into()
            } else {
                m.state.clone()
            },
            ..m.clone()
        };
        strip(self) != strip(other)
    }

    fn claim_differs(&self, other: &Self) -> bool {
        self.claimers != other.claimers
            || self.claimed_ms.is_some() != other.claimed_ms.is_some()
            || (self.state == "claimed") != (other.state == "claimed")
    }

    /// The row a peer stores for it: no diff, and a code review's
    /// description left for the owner to give when it is read.
    pub fn row(&self) -> ReviewRow {
        ReviewRow {
            id: self.id.clone(),
            session_id: self.session_id.clone(),
            node_id: self.node_id.clone(),
            channel: self.channel.clone(),
            kind: self.kind.clone(),
            title: self.title.clone(),
            body: self.body.clone().unwrap_or_default(),
            edited_title: None,
            edited_body: None,
            provider: self.provider.clone(),
            target: self.target.clone(),
            diff: String::new(),
            files: self.files.clone(),
            head_sha: self.head_sha.clone(),
            base_ref: self.base_ref.clone(),
            added: self.added,
            removed: self.removed,
            state: self.state.clone(),
            verdict_reason: self.verdict_reason.clone(),
            publish_result: None,
            claimed_ms: self.claimed_ms,
            created_ms: self.created_ms,
            created_mono_ms: 0,
            resolved_mono_ms: None,
            updated_ms: self.updated_ms,
            checks_json: None,
            review_session_id: None,
            ai_verdict_json: None,
            revision_patch: None,
            lane: self.lane.clone(),
            publication_failed: self.publication_failed.clone(),
        }
    }
}

/// What became of a review that left its owner's queue.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReviewClosed {
    pub id: String,
    pub state: String,
    #[serde(default)]
    pub verdict_reason: Option<String>,
    #[serde(default)]
    pub publish_result: Option<String>,
    pub updated_ms: i64,
}

impl ReviewClosed {
    pub fn of(r: &ReviewRow) -> Self {
        Self {
            id: r.id.clone(),
            state: r.state.clone(),
            verdict_reason: r.verdict_reason.clone(),
            publish_result: r.publish_result.clone(),
            updated_ms: r.updated_ms,
        }
    }

    /// One the owner no longer holds at all.
    pub fn lost(id: &str) -> Self {
        Self {
            id: id.to_string(),
            state: "gone".into(),
            verdict_reason: None,
            publish_result: None,
            updated_ms: crate::store::now_ms(),
        }
    }
}

/// The events that take a channel from what it was last told (`sent`) to
/// what the owner lists now (`now`), each with its channel. `closed` says
/// what became of one that left the listing, if the owner still holds it.
pub fn events(
    sent: &HashMap<String, ReviewMeta>,
    now: &[ReviewMeta],
    closed: impl Fn(&str) -> Option<ReviewClosed>,
) -> Vec<(String, ReviewEvent)> {
    let mut out = Vec::new();
    let value = |m: &ReviewMeta| serde_json::to_value(m).unwrap_or(Value::Null);
    for m in now {
        match sent.get(&m.id) {
            None => out.push((m.channel.clone(), ReviewEvent::Opened { review: value(m) })),
            Some(before) if m.listing_differs(before) => {
                out.push((m.channel.clone(), ReviewEvent::Revised { review: value(m) }))
            }
            Some(before) if m.claim_differs(before) => {
                let gained = m.claimers.iter().any(|c| !before.claimers.contains(c))
                    || (m.claimed_ms.is_some() && before.claimed_ms.is_none());
                let (review_id, claimers, claimed_ms) =
                    (m.id.clone(), m.claimers.clone(), m.claimed_ms);
                out.push((
                    m.channel.clone(),
                    if gained {
                        ReviewEvent::Claimed {
                            review_id,
                            claimers,
                            claimed_ms,
                        }
                    } else {
                        ReviewEvent::Unclaimed {
                            review_id,
                            claimers,
                            claimed_ms,
                        }
                    },
                ));
            }
            Some(_) => {}
        }
    }
    let listed: BTreeSet<&str> = now.iter().map(|m| m.id.as_str()).collect();
    let mut gone: Vec<&ReviewMeta> = sent
        .values()
        .filter(|m| !listed.contains(m.id.as_str()))
        .collect();
    gone.sort_by(|a, b| a.id.cmp(&b.id));
    for m in gone {
        let outcome = closed(&m.id).unwrap_or_else(|| ReviewClosed::lost(&m.id));
        out.push((
            m.channel.clone(),
            ReviewEvent::Closed {
                outcome: serde_json::to_value(outcome).unwrap_or(Value::Null),
            },
        ));
    }
    out
}

/// One peer watching a review this node owns.
#[derive(Clone, Debug, Default)]
pub struct Watcher {
    pub expires_ms: i64,
    /// The head whose diff the watcher holds, so a push can leave it out.
    pub sent_head: Option<String>,
    /// What the last detail sent hashed to, so an unchanged one is not sent.
    pub sent_hash: Option<u64>,
}

/// A peer's review this node is watching: the owner's detail as last read
/// or pushed.
#[derive(Clone, Debug)]
struct Held {
    owner: String,
    detail: Value,
    expires_ms: i64,
}

/// Watches both ways: peers watching this node's reviews, and this node
/// watching peers'.
#[derive(Default)]
pub struct Watches {
    watchers: Mutex<HashMap<String, HashMap<String, Watcher>>>,
    held: Mutex<HashMap<String, Held>>,
}

impl Watches {
    // ------------------------------------------------------------- owner

    /// `node` watches `review` from `now`, or renews. Whether it is new.
    pub fn watch(&self, review: &str, node: &str, now: i64) -> bool {
        let mut w = self.watchers.lock().unwrap();
        let slot = w.entry(review.to_string()).or_default();
        let fresh = !slot.contains_key(node);
        slot.entry(node.to_string()).or_default().expires_ms = now + WATCH_TTL_MS;
        fresh
    }

    /// `node` stops watching `review`. Whether it was.
    pub fn unwatch(&self, review: &str, node: &str) -> bool {
        let mut w = self.watchers.lock().unwrap();
        let Some(slot) = w.get_mut(review) else {
            return false;
        };
        let was = slot.remove(node).is_some();
        if slot.is_empty() {
            w.remove(review);
        }
        was
    }

    /// Nobody watches `review` any more: it left the queue. Who did.
    pub fn end(&self, review: &str) -> Vec<String> {
        let mut w = self.watchers.lock().unwrap();
        w.remove(review)
            .map(|slot| slot.into_keys().collect())
            .unwrap_or_default()
    }

    /// Who watches `review`, sorted.
    pub fn claimers(&self, review: &str) -> Vec<String> {
        let w = self.watchers.lock().unwrap();
        let mut out: Vec<String> = w
            .get(review)
            .map(|slot| slot.keys().cloned().collect())
            .unwrap_or_default();
        out.sort();
        out
    }

    /// The reviews anyone watches.
    pub fn watched(&self) -> Vec<String> {
        self.watchers.lock().unwrap().keys().cloned().collect()
    }

    pub fn watchers(&self, review: &str) -> Vec<(String, Watcher)> {
        self.watchers
            .lock()
            .unwrap()
            .get(review)
            .map(|slot| slot.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    }

    /// What was last sent to `node` for `review`.
    pub fn sent(&self, review: &str, node: &str, head: &str, hash: u64) {
        if let Some(w) = self
            .watchers
            .lock()
            .unwrap()
            .get_mut(review)
            .and_then(|slot| slot.get_mut(node))
        {
            w.sent_head = Some(head.to_string());
            w.sent_hash = Some(hash);
        }
    }

    /// Drop watches that lapsed or whose watcher cannot be reached. The
    /// reviews whose watchers changed.
    pub fn expire(&self, now: i64, reachable: impl Fn(&str) -> bool) -> Vec<String> {
        let mut w = self.watchers.lock().unwrap();
        let mut changed = Vec::new();
        w.retain(|review, slot| {
            let before = slot.len();
            slot.retain(|node, watcher| watcher.expires_ms > now && reachable(node));
            if slot.len() != before {
                changed.push(review.clone());
            }
            !slot.is_empty()
        });
        changed
    }

    // -------------------------------------------------------------- peer

    /// The owner's detail of `review`, read now, held until the watch lapses.
    pub fn hold(&self, review: &str, owner: &str, detail: Value, now: i64) {
        self.held.lock().unwrap().insert(
            review.to_string(),
            Held {
                owner: owner.to_string(),
                detail,
                expires_ms: now + WATCH_TTL_MS,
            },
        );
    }

    /// Renew this node's watch of `review` here. Whether one is held.
    pub fn renew_held(&self, review: &str, now: i64) -> bool {
        match self.held.lock().unwrap().get_mut(review) {
            Some(h) => {
                h.expires_ms = now + WATCH_TTL_MS;
                true
            }
            None => false,
        }
    }

    /// The owner's detail of `review`, while this node's watch holds.
    pub fn held(&self, review: &str, now: i64) -> Option<Value> {
        let mut held = self.held.lock().unwrap();
        match held.get(review) {
            Some(h) if h.expires_ms > now => Some(h.detail.clone()),
            Some(_) => {
                held.remove(review);
                None
            }
            None => None,
        }
    }

    /// Take a pushed detail onto what is held. A push that left the diff out
    /// keeps the one held. Whether it was taken: only from the owner, and
    /// only while watching.
    pub fn take_push(&self, review: &str, owner: &str, mut detail: Value) -> bool {
        let mut held = self.held.lock().unwrap();
        let Some(h) = held.get_mut(review).filter(|h| h.owner == owner) else {
            return false;
        };
        if detail["diff_omitted"].as_bool() == Some(true) {
            detail["review"]["diff"] = h.detail["review"]["diff"].clone();
            if let Some(o) = detail.as_object_mut() {
                o.remove("diff_omitted");
            }
            if let Some(t) = h.detail.get("diff_truncated") {
                detail["diff_truncated"] = t.clone();
            }
        }
        h.detail = detail;
        true
    }

    /// Stop holding `review`. Whether it was held.
    pub fn release_held(&self, review: &str) -> bool {
        self.held.lock().unwrap().remove(review).is_some()
    }
}

/// Hash a detail for "has it changed".
pub fn hash(v: &Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(v).unwrap_or_default().hash(&mut h);
    h.finish()
}

/// The most a review's diff may take in a frame: the rest of the detail and
/// the sealing (base64 adds a third) must still fit under
/// [`proto::frame::MAX_FRAME_BYTES`].
pub const DIFF_FRAME_BUDGET: usize = 2 * 1024 * 1024;

/// Cut a detail's diff to [`DIFF_FRAME_BUDGET`], at a line, saying so in
/// `diff_truncated: { bytes, shown }`, so a review too large for a frame is
/// still shown, with its owner named as where to read the rest.
pub fn truncate_diff(detail: &mut Value) {
    let Some(diff) = detail["review"]["diff"].as_str() else {
        return;
    };
    if diff.len() <= DIFF_FRAME_BUDGET {
        return;
    }
    let mut cut = DIFF_FRAME_BUDGET;
    while !diff.is_char_boundary(cut) {
        cut -= 1;
    }
    let cut = diff[..cut].rfind('\n').map(|i| i + 1).unwrap_or(cut);
    let total = diff.len();
    let shown = diff[..cut].to_string();
    detail["review"]["diff"] = Value::String(shown);
    detail["diff_truncated"] = serde_json::json!({ "bytes": total, "shown": cut });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str) -> ReviewRow {
        ReviewRow {
            id: id.into(),
            session_id: None,
            node_id: "owner".into(),
            channel: "personal".into(),
            kind: "pr".into(),
            title: "feat: x".into(),
            body: "a long description".into(),
            edited_title: None,
            edited_body: Some("edited".into()),
            provider: "github".into(),
            target: "{}".into(),
            diff: "+secret diff".into(),
            files: "[]".into(),
            head_sha: "a1".into(),
            base_ref: "main".into(),
            added: 1,
            removed: 0,
            state: "new".into(),
            verdict_reason: None,
            publish_result: None,
            claimed_ms: None,
            created_ms: 1,
            created_mono_ms: 0,
            resolved_mono_ms: None,
            updated_ms: 1,
            checks_json: Some("[{\"ok\":true}]".into()),
            review_session_id: None,
            ai_verdict_json: None,
            revision_patch: None,
            lane: None,
            publication_failed: None,
        }
    }

    fn sent(metas: &[ReviewMeta]) -> HashMap<String, ReviewMeta> {
        metas.iter().map(|m| (m.id.clone(), m.clone())).collect()
    }

    #[test]
    fn opened_revised_and_closed_are_one_compact_event_each() {
        let r = ReviewMeta::of(&row("r1"), vec![]);
        let ev = events(&HashMap::new(), std::slice::from_ref(&r), |_| None);
        assert_eq!(ev.len(), 1);
        let json = serde_json::to_string(&ev[0].1).unwrap();
        assert!(json.contains("\"event\":\"opened\""), "{json}");
        for leaked in ["secret diff", "a long description", "edited", "\"ok\""] {
            assert!(!json.contains(leaked), "{leaked} in {json}");
        }

        // Nothing changed: nothing sent.
        assert!(events(
            &sent(std::slice::from_ref(&r)),
            std::slice::from_ref(&r),
            |_| None
        )
        .is_empty());
        // A row touched without a listed change: still nothing.
        let touched = ReviewMeta {
            updated_ms: 9,
            ..r.clone()
        };
        assert!(events(&sent(std::slice::from_ref(&r)), &[touched], |_| None).is_empty());

        // Resubmitted.
        let revised = ReviewMeta {
            head_sha: "b2".into(),
            added: 5,
            ..r.clone()
        };
        let ev = events(
            &sent(std::slice::from_ref(&r)),
            std::slice::from_ref(&revised),
            |_| None,
        );
        assert_eq!(ev.len(), 1);
        assert!(matches!(&ev[0].1, ReviewEvent::Revised { review } if review["head_sha"] == "b2"));

        // Decided.
        let mut decided = row("r1");
        decided.state = "approved".into();
        decided.publish_result = Some("https://pr".into());
        let ev = events(&sent(&[revised]), &[], |id| {
            (id == "r1").then(|| ReviewClosed::of(&decided))
        });
        assert_eq!(ev.len(), 1);
        let json = serde_json::to_string(&ev[0].1).unwrap();
        assert!(
            json.contains("\"event\":\"closed\"") && json.contains("https://pr"),
            "{json}"
        );
        assert!(
            !json.contains("secret diff") && !json.contains("edited"),
            "{json}"
        );

        // Lost: closed as gone.
        let ev = events(&sent(&[r]), &[], |_| None);
        assert!(matches!(&ev[0].1, ReviewEvent::Closed { outcome } if outcome["state"] == "gone"));
    }

    #[test]
    fn a_watcher_arriving_or_leaving_is_a_claim_not_a_revision() {
        let r = ReviewMeta::of(&row("r1"), vec![]);
        let watched = ReviewMeta::of(&row("r1"), vec!["peer".into()]);
        let ev = events(
            &sent(std::slice::from_ref(&r)),
            std::slice::from_ref(&watched),
            |_| None,
        );
        assert!(
            matches!(&ev[..], [(_, ReviewEvent::Claimed { claimers, .. })] if claimers == &["peer".to_string()]),
            "{ev:?}"
        );
        let ev = events(&sent(&[watched]), std::slice::from_ref(&r), |_| None);
        assert!(
            matches!(&ev[..], [(_, ReviewEvent::Unclaimed { claimers, .. })] if claimers.is_empty())
        );
        // The owner's own claim (state and time) is a claim too.
        let mut claimed = row("r1");
        claimed.state = "claimed".into();
        claimed.claimed_ms = Some(5);
        let ev = events(&sent(&[r]), &[ReviewMeta::of(&claimed, vec![])], |_| None);
        assert!(
            matches!(
                &ev[..],
                [(
                    _,
                    ReviewEvent::Claimed {
                        claimed_ms: Some(5),
                        ..
                    }
                )]
            ),
            "{ev:?}"
        );
    }

    #[test]
    fn a_report_carries_its_text_and_a_code_review_does_not() {
        let mut report = row("r");
        report.kind = reports::KIND.into();
        assert_eq!(
            ReviewMeta::of(&report, vec![]).body.as_deref(),
            Some("a long description")
        );
        assert_eq!(ReviewMeta::of(&row("c"), vec![]).body, None);
        assert_eq!(ReviewMeta::of(&row("c"), vec![]).row().diff, "");
    }

    #[test]
    fn a_watch_lapses_unless_renewed_and_when_its_watcher_is_unreachable() {
        let w = Watches::default();
        assert!(w.watch("r", "a", 0));
        assert!(w.watch("r", "b", 0));
        assert!(!w.watch("r", "a", 0));
        assert_eq!(w.claimers("r"), vec!["a".to_string(), "b".to_string()]);
        // Renewed halfway, `a` outlives `b`.
        assert!(!w.watch("r", "a", WATCH_TTL_MS / 2));
        assert_eq!(w.expire(WATCH_TTL_MS + 1, |_| true), vec!["r".to_string()]);
        assert_eq!(w.claimers("r"), vec!["a".to_string()]);
        // Unreachable, it goes at once.
        assert_eq!(
            w.expire(WATCH_TTL_MS + 2, |n| n != "a"),
            vec!["r".to_string()]
        );
        assert!(w.claimers("r").is_empty());
        assert!(w.watched().is_empty());
        assert!(w.expire(WATCH_TTL_MS + 3, |_| true).is_empty());
    }

    #[test]
    fn held_detail_lapses_and_a_push_keeps_the_diff_it_left_out() {
        let w = Watches::default();
        let detail = serde_json::json!({ "review": { "diff": "+d", "ai_verdict_json": null } });
        w.hold("r", "owner", detail, 0);
        // Only the owner's push is taken.
        assert!(!w.take_push("r", "intruder", serde_json::json!({})));
        let push = serde_json::json!({
            "review": { "diff": null, "ai_verdict_json": "{\"verdict\":\"approve\"}" },
            "diff_omitted": true,
        });
        assert!(w.take_push("r", "owner", push));
        let held = w.held("r", 1).unwrap();
        assert_eq!(held["review"]["diff"], "+d");
        assert_eq!(
            held["review"]["ai_verdict_json"],
            "{\"verdict\":\"approve\"}"
        );
        assert!(held.get("diff_omitted").is_none());
        assert!(w.renew_held("r", WATCH_TTL_MS - 1));
        assert!(w.held("r", WATCH_TTL_MS + 1).is_some());
        assert!(w.held("r", 2 * WATCH_TTL_MS).is_none());
        assert!(!w.take_push("r", "owner", serde_json::json!({})));
    }

    #[test]
    fn a_diff_too_large_for_a_frame_is_cut_at_a_line_and_says_so() {
        let line = "+".repeat(99) + "\n";
        let big = line.repeat(DIFF_FRAME_BUDGET / 100 + 10);
        let mut d = serde_json::json!({ "review": { "diff": big } });
        truncate_diff(&mut d);
        let shown = d["review"]["diff"].as_str().unwrap();
        assert!(shown.len() <= DIFF_FRAME_BUDGET && shown.ends_with('\n'));
        assert_eq!(d["diff_truncated"]["bytes"], big.len());
        let mut small = serde_json::json!({ "review": { "diff": "+x\n" } });
        truncate_diff(&mut small);
        assert!(small.get("diff_truncated").is_none());
    }
}
