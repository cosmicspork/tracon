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
    /// Turns on this channel today the gateway could not count. They
    /// contribute nothing to `usage_today` — which is exactly why they are
    /// reported separately: a ceiling read without them is a ceiling that has
    /// silently stopped measuring part of the day.
    #[serde(default)]
    pub unmetered_turns: i64,
}

impl CeilingInfo {
    pub fn at(&self) -> bool {
        self.state == "at"
    }
    pub fn reason(&self) -> String {
        let mut reason = format!(
            "{} of {} tokens today",
            self.usage_today,
            self.ceiling.unwrap_or(0)
        );
        if self.unmetered_turns > 0 {
            reason.push_str(&format!(
                ", plus {} unmetered {} this node could not count",
                self.unmetered_turns,
                if self.unmetered_turns == 1 {
                    "turn"
                } else {
                    "turns"
                }
            ));
        }
        reason
    }
}

/// Today's spend against the channel's ceiling.
pub fn ceiling(store: &Store, bindings: &Value, channel: &str) -> CeilingInfo {
    let since = crate::corpus::promote::chrono_free::day_start_ms();
    let usage_today = store.usage_tokens_since(channel, since).unwrap_or(0);
    let unmetered_turns = store.unmetered_turns_since(channel, since).unwrap_or(0);
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
        unmetered_turns,
    }
}

// ---- two usage sources, one ledger ---------------------------------------
//
// The gateway counts every model call on the wire and that count is what the
// budget and the channel ceiling are charged against. The harness reports its
// own usage per turn — OpenCode's `tokens.{input,output,reasoning,cache.*}`
// and `cost`, omp's ACP `usage` — and a harness that reports nothing reports
// zero rather than "unknown" (`docs/reference/opencode-v1.18.30/providers.md`
// §6.3). So the two are recorded side by side and compared, and the harness
// number is never allowed to lower the charge.

/// The verdict on one turn's two numbers.
pub mod usage_state {
    /// The two sources agree within tolerance (both zero counts as agreeing).
    pub const RECONCILED: &str = "reconciled";
    /// They disagree beyond tolerance, or the harness reported nothing while
    /// the gateway counted something.
    pub const MISMATCH: &str = "mismatch";
    /// The gateway forwarded model calls and could count none of them: the
    /// provider returned no usage and there is no estimator. Not zero, and
    /// not free — unknown, and said so.
    pub const UNMETERED: &str = "unmetered";
}

/// How far the two sources may differ before it is worth telling the operator.
/// They legitimately count different things at the edges — cache accounting,
/// a retried request the harness folded into one turn — so the tolerance is
/// proportional, with a floor for small turns.
const USAGE_TOLERANCE_FRACTION: f64 = 0.1;
const USAGE_TOLERANCE_TOKENS: i64 = 200;

/// One turn reconciled: both numbers, what was charged, and the verdict.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Reconciled {
    pub turn: i64,
    pub state: &'static str,
    pub gateway_input: i64,
    pub gateway_output: i64,
    pub gateway_tokens: i64,
    pub gateway_requests: i64,
    /// `None` when the harness said nothing at all about this turn.
    pub harness_tokens: Option<i64>,
    pub harness_cost_usd: Option<f64>,
    /// What the budget is charged: never less than the gateway counted.
    pub charged: i64,
}

impl Reconciled {
    pub fn agreed(&self) -> bool {
        self.state == usage_state::RECONCILED
    }
    pub fn unmetered(&self) -> bool {
        self.state == usage_state::UNMETERED
    }
    /// The event this verdict is worth writing to the session log, if any. A
    /// turn whose two sources agree is the ordinary case and says nothing.
    pub fn event_kind(&self) -> Option<&'static str> {
        use crate::session::state::event_kind as ek;
        match self.state {
            usage_state::MISMATCH => Some(ek::USAGE_MISMATCH),
            usage_state::UNMETERED => Some(ek::USAGE_UNMETERED),
            _ => None,
        }
    }

    /// Both sides, for the event payload and the API.
    pub fn detail(&self) -> Value {
        json!({
            "turn": self.turn,
            "state": self.state,
            "gateway": {
                "input_tokens": self.gateway_input,
                "output_tokens": self.gateway_output,
                "tokens": self.gateway_tokens,
                "requests": self.gateway_requests,
            },
            "harness": {
                "tokens": self.harness_tokens,
                "cost_usd": self.harness_cost_usd,
            },
            "charged_tokens": self.charged,
        })
    }
}

/// The rule, with no store in the way so it can be read and tested as one
/// thing. `gateway` is `(input, output, requests)` as counted on the wire.
pub fn reconcile(
    turn: i64,
    gateway: (i64, i64, i64),
    harness_tokens: Option<i64>,
    harness_cost_usd: Option<f64>,
) -> Reconciled {
    let (gateway_input, gateway_output, gateway_requests) = gateway;
    let gateway_tokens = gateway_input + gateway_output;
    let harness = harness_tokens.unwrap_or(0).max(0);
    // The harness is display; it can raise the charge (it may have seen a call
    // the gateway did not) but never lower it.
    let charged = gateway_tokens.max(harness);
    let tolerance = USAGE_TOLERANCE_TOKENS
        .max((gateway_tokens.max(harness) as f64 * USAGE_TOLERANCE_FRACTION) as i64);
    let state = if gateway_requests > 0 && gateway_tokens == 0 {
        usage_state::UNMETERED
    } else if gateway_tokens > 0 && harness == 0 {
        usage_state::MISMATCH
    } else if (gateway_tokens - harness).abs() <= tolerance {
        usage_state::RECONCILED
    } else {
        usage_state::MISMATCH
    };
    Reconciled {
        turn,
        state,
        gateway_input,
        gateway_output,
        gateway_tokens,
        gateway_requests,
        harness_tokens,
        harness_cost_usd,
        charged,
    }
}

/// Close a turn's ledger row: read what the gateway counted for it, weigh it
/// against what the harness reported, write both down with the verdict, and
/// hand back what the budget should be charged.
pub fn settle_turn(
    store: &Store,
    session_id: &str,
    harness_tokens: Option<i64>,
    harness_cost_usd: Option<f64>,
) -> Reconciled {
    let turn = store.current_turn(session_id).unwrap_or(0);
    let gateway = store
        .turn_gateway_counts(session_id, turn)
        .unwrap_or((0, 0, 0));
    let out = reconcile(turn, gateway, harness_tokens, harness_cost_usd);
    if let Err(e) = store.settle_turn(
        session_id,
        turn,
        gateway,
        harness_tokens,
        harness_cost_usd,
        out.charged,
        out.state,
    ) {
        tracing::warn!(error = %e, session_id, turn, "turn usage not recorded");
    }
    out
}

/// What the session API and the SPA show: the ledger, and the standing state
/// of the last turn that settled.
pub fn session_usage(store: &Store, session_id: &str) -> Value {
    let turns = store.turn_ledger(session_id, 50).unwrap_or_default();
    let settled = turns.iter().find(|t| t.state != "open");
    let gateway_tokens: i64 = turns.iter().map(|t| t.gateway_tokens()).sum();
    let harness_tokens: i64 = turns.iter().filter_map(|t| t.harness_tokens).sum();
    json!({
        "state": settled.map(|t| t.state.clone()),
        "gateway_tokens": gateway_tokens,
        "harness_tokens": harness_tokens,
        "charged_tokens": turns.iter().map(|t| t.charged_tokens).sum::<i64>(),
        "unmetered_turns": turns.iter().filter(|t| t.state == usage_state::UNMETERED).count(),
        "mismatched_turns": turns.iter().filter(|t| t.state == usage_state::MISMATCH).count(),
        "turns": turns,
    })
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
    #[serde(flatten)]
    pub workflow: crate::store::metrics::WorkflowMetrics,
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
    let mut workflow = store.workflow_metrics(channel, since_ms)?;
    workflow.interventions += answers;
    workflow.human_wait_seconds += human_perm;
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
        workflow,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_sources_that_agree_are_reconciled_and_charged_once() {
        let r = reconcile(1, (900, 100, 3), Some(1000), Some(0.02));
        assert_eq!(r.state, usage_state::RECONCILED);
        assert_eq!(r.charged, 1000);
        assert_eq!(r.gateway_tokens, 1000);
        // Both numbers survive into the record; neither is discarded.
        assert_eq!(r.detail()["gateway"]["tokens"], 1000);
        assert_eq!(r.detail()["harness"]["tokens"], 1000);
    }

    #[test]
    fn a_turn_with_no_model_calls_agrees_at_zero() {
        let r = reconcile(1, (0, 0, 0), Some(0), None);
        assert_eq!(r.state, usage_state::RECONCILED);
        assert_eq!(r.charged, 0);
    }

    #[test]
    fn small_differences_are_within_tolerance_and_large_ones_are_not() {
        assert!(reconcile(1, (500, 500, 2), Some(1100), None).agreed());
        assert_eq!(
            reconcile(1, (5_000, 5_000, 2), Some(4_000), None).state,
            usage_state::MISMATCH
        );
    }

    #[test]
    fn a_harness_that_under_reports_never_lowers_the_charge() {
        let r = reconcile(1, (8_000, 2_000, 4), Some(10), None);
        assert_eq!(r.state, usage_state::MISMATCH);
        assert_eq!(r.charged, 10_000, "the gateway's count is what is charged");
    }

    #[test]
    fn a_silent_harness_beside_a_counting_gateway_is_a_mismatch() {
        let r = reconcile(1, (40, 10, 1), None, None);
        assert_eq!(r.state, usage_state::MISMATCH);
        assert_eq!(r.charged, 50);
        assert_eq!(r.detail()["harness"]["tokens"], Value::Null);
    }

    #[test]
    fn calls_the_gateway_could_not_count_are_unmetered_not_free() {
        let r = reconcile(1, (0, 0, 3), Some(0), None);
        assert_eq!(r.state, usage_state::UNMETERED);
        assert!(r.unmetered());
        assert_eq!(r.gateway_requests, 3);
    }

    #[test]
    fn a_harness_that_saw_more_than_the_gateway_raises_the_charge() {
        let r = reconcile(1, (100, 100, 1), Some(9_000), None);
        assert_eq!(r.charged, 9_000);
        assert_eq!(r.state, usage_state::MISMATCH);
    }

    #[test]
    fn unmetered_turns_are_named_in_the_ceiling_reason() {
        let info = CeilingInfo {
            usage_today: 40,
            ceiling: Some(100),
            state: "under".into(),
            unmetered_turns: 2,
        };
        assert!(
            info.reason().contains("2 unmetered turns"),
            "{}",
            info.reason()
        );
        let none = CeilingInfo {
            unmetered_turns: 0,
            ..info
        };
        assert_eq!(none.reason(), "40 of 100 tokens today");
    }
}
