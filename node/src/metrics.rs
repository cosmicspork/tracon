//! The numbers that matter at six months, computed from what the node
//! already records: approvals per accepted change, tokens per accepted
//! change (priced where a provider carries a price), human and agent time.
//! Plus the per-channel daily ceiling, and provenance per commit.
//!
//! Every figure is "as seen from this node": sessions and events mirror
//! across the mesh, gateway usage is counted where the call was made.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::Config;
use crate::store::{ReviewRow, SessionRow, Store};

/// Bindings key for the ceiling: tokens (input + output, as the gateway
/// counts them) a channel may spend per local day.
pub const CEILING_KEY: &str = "ceiling_tokens_per_day";
/// Above this fraction of the ceiling the channel is "near".
const NEAR: f64 = 0.8;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CeilingInfo {
    pub usage_today: i64,
    pub ceiling: Option<i64>,
    /// `under`, `near`, `at`, or `none` when no ceiling is bound.
    pub state: String,
}

impl CeilingInfo {
    pub fn at(&self) -> bool {
        self.state == "at"
    }
    pub fn reason(&self) -> String {
        format!(
            "{} of {} tokens today",
            self.usage_today,
            self.ceiling.unwrap_or(0)
        )
    }
}

/// Today's spend against the channel's ceiling.
pub fn ceiling(store: &Store, bindings: &Value, channel: &str) -> CeilingInfo {
    let usage_today = store
        .usage_tokens_since(channel, crate::corpus::promote::chrono_free::day_start_ms())
        .unwrap_or(0);
    let ceiling = bindings[CEILING_KEY].as_i64().filter(|c| *c > 0);
    let state = match ceiling {
        None => "none",
        Some(c) if usage_today >= c => "at",
        Some(c) if usage_today as f64 >= c as f64 * NEAR => "near",
        Some(_) => "under",
    };
    CeilingInfo {
        usage_today,
        ceiling,
        state: state.into(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChannelMetrics {
    pub channel: String,
    pub since_ms: i64,
    /// Reviews approved and published.
    pub accepted_changes: i64,
    pub rejected_changes: i64,
    /// Permission answers plus review verdicts.
    pub approvals: i64,
    pub approvals_per_accepted_change: Option<f64>,
    /// Gateway tokens of the sessions behind accepted changes (the
    /// implementing and review sessions), over accepted changes.
    pub tokens_per_accepted_change: Option<f64>,
    /// All gateway tokens on the channel in the window.
    pub tokens: i64,
    /// Priced where the provider carries a price; null otherwise.
    pub cost_usd: Option<f64>,
    pub human_seconds: f64,
    pub agent_seconds: f64,
    pub sessions: i64,
}

pub fn channel_metrics(
    store: &Store,
    cfg: &Config,
    channel: &str,
    since_ms: i64,
) -> Result<ChannelMetrics, crate::store::StoreError> {
    let reviews = store.reviews_decided_since(channel, since_ms)?;
    let accepted: Vec<&ReviewRow> = reviews.iter().filter(|r| r.state == "approved").collect();
    let rejected = reviews.len() as i64 - accepted.len() as i64;
    let (answers, human_perm) = store.permissions_answered_since(channel, since_ms)?;
    let approvals = answers + reviews.len() as i64;
    // Human time on reviews: claim to decision, on the node that decided.
    let human_reviews: f64 = reviews
        .iter()
        .filter_map(|r| r.claimed_ms.map(|c| (r.updated_ms - c).max(0)))
        .sum::<i64>() as f64
        / 1000.0;
    let usage = store.usage_by_session(channel, since_ms)?;
    let tokens: i64 = usage.iter().map(|(_, _, i, o, _)| i + o).sum();
    let mut cost = None;
    for (_, provider, i, o, _) in &usage {
        if let Some(price) = cfg.providers.get(provider).and_then(|p| p.price) {
            *cost.get_or_insert(0.0) += price.cost(*i, *o);
        }
    }
    let behind_accepted: std::collections::HashSet<&str> = accepted
        .iter()
        .flat_map(|r| std::iter::once(r.session_id.as_str()).chain(r.review_session_id.as_deref()))
        .collect();
    let accepted_tokens: i64 = usage
        .iter()
        .filter(|(sid, ..)| behind_accepted.contains(sid.as_str()))
        .map(|(_, _, i, o, _)| i + o)
        .sum();
    let sessions = store.sessions_on_channel_since(channel, since_ms)?;
    let agent_seconds: f64 = sessions
        .iter()
        .filter_map(|s| match (s.started_mono_ms, s.ended_mono_ms) {
            (Some(a), Some(b)) => Some((b - a).max(0)),
            _ => None,
        })
        .sum::<i64>() as f64
        / 1000.0;
    let n = accepted.len() as i64;
    let per = |x: f64| (n > 0).then(|| x / n as f64);
    Ok(ChannelMetrics {
        channel: channel.into(),
        since_ms,
        accepted_changes: n,
        rejected_changes: rejected,
        approvals,
        approvals_per_accepted_change: per(approvals as f64),
        tokens_per_accepted_change: per(accepted_tokens as f64),
        tokens,
        cost_usd: cost,
        human_seconds: human_perm + human_reviews,
        agent_seconds,
        sessions: sessions.len() as i64,
    })
}

/// Which model, which prompt, which approval, which policy version, for a
/// commit that shipped under the operator's name.
pub fn provenance(store: &Store, sha: &str) -> Result<Option<Value>, crate::store::StoreError> {
    let Some(review) = store.review_by_sha(sha)? else {
        return Ok(None);
    };
    let session = store.get_session(&review.session_id)?;
    let review_session = review
        .review_session_id
        .as_deref()
        .and_then(|id| store.get_session(id).ok().flatten());
    let item = session
        .as_ref()
        .and_then(|s| s.work_item_id.as_deref())
        .and_then(|id| store.work_get(id).ok().flatten());
    let events = store.events_after(&review.session_id, 0, 5000)?;
    let prompts: Vec<Value> = events
        .iter()
        .filter(|e| e.kind == "user_prompt")
        .map(|e| json!({ "at_ms": e.at_ms, "text": e.payload["text"] }))
        .collect();
    let answers: Vec<Value> = events
        .iter()
        .filter(|e| {
            e.kind == "permission_answer" || e.kind == "policy_allowed" || e.kind == "policy_denied"
        })
        .map(|e| json!({ "at_ms": e.at_ms, "kind": e.kind, "payload": e.payload }))
        .collect();
    let checks: Vec<Value> = review
        .checks_json
        .as_deref()
        .and_then(|c| serde_json::from_str(c).ok())
        .unwrap_or_default();
    let ai_verdict: Option<Value> = review
        .ai_verdict_json
        .as_deref()
        .and_then(|v| serde_json::from_str(v).ok());
    let session_view = |s: &SessionRow| {
        json!({
            "id": s.id, "node_id": s.node_id, "model": s.model, "phase": s.phase,
            "policy_version": s.policy_version, "budget_tokens": s.budget_tokens,
            "tokens_used": s.tokens_used, "created_ms": s.created_ms, "end_reason": s.end_reason,
        })
    };
    Ok(Some(json!({
        "sha": review.head_sha,
        "published": review.publish_result,
        "review": {
            "id": review.id, "state": review.state, "title": review.approved_title(),
            "base_ref": review.base_ref, "added": review.added, "removed": review.removed,
            "decided_ms": review.updated_ms, "decided_on": review.node_id,
            "edited": review.edited_title.is_some() || review.edited_body.is_some(),
            "verdict_reason": review.verdict_reason,
        },
        "work_item": item.map(|i| json!({ "id": i.id, "title": i.title, "state": i.state, "plan": i.phase_plan_slug })),
        "implementing_session": session.as_ref().map(session_view),
        "review_session": review_session.as_ref().map(session_view),
        "ai_verdict": ai_verdict,
        "policy_version": session.as_ref().and_then(|s| s.policy_version),
        "prompts": prompts,
        "approvals": answers,
        "checks": checks,
    })))
}
