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
use crate::store::{RequestMatch, ReviewRow, SessionRow, Store};

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

// ---- infrastructure: sandboxes by the hour ---------------------------------

/// Today's infrastructure spend against `[spend] infra_usd_per_day`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InfraSpend {
    /// Ended sandboxes' cost today plus running ones' so far. Unpriced
    /// sandboxes add nothing, and are counted in `unpriced`.
    pub today_usd: f64,
    pub limit_usd: Option<f64>,
    /// `under`, `near`, `at`, or `none` when no limit is set.
    pub state: String,
    pub running: i64,
    pub unpriced: i64,
}

impl InfraSpend {
    pub fn at(&self) -> bool {
        self.state == "at"
    }
    pub fn reason(&self) -> String {
        format!(
            "today's infrastructure spend ${:.2} reached [spend] infra_usd_per_day ${:.2}",
            self.today_usd,
            self.limit_usd.unwrap_or(0.0)
        )
    }
}

/// Today's spend on sandboxes, on every channel, against the node's limit.
pub fn infra_spend(store: &Store, cfg: &Config) -> InfraSpend {
    let since = crate::corpus::promote::chrono_free::day_start_ms();
    infra_spend_since(
        store,
        cfg.live().spend.infra_limit(),
        since,
        crate::store::now_ms(),
    )
}

fn infra_spend_since(store: &Store, limit: Option<f64>, since: i64, now: i64) -> InfraSpend {
    let rows = store
        .sandboxes_between(since, None, None)
        .unwrap_or_default();
    let today_usd: f64 = rows
        .iter()
        .filter_map(|r| r.cost_between(since, None, now))
        .sum();
    let state = match limit {
        None => "none",
        Some(l) if today_usd >= l => "at",
        Some(l) if today_usd >= l * NEAR => "near",
        Some(_) => "under",
    };
    InfraSpend {
        today_usd,
        limit_usd: limit,
        state: state.into(),
        running: rows.iter().filter(|r| r.ended_ms.is_none()).count() as i64,
        unpriced: rows
            .iter()
            .filter(|r| r.price_per_hour_usd.is_none())
            .count() as i64,
    }
}

/// What one session's sandboxes cost: the runtime half of its cost, beside
/// the model half (`cost_usd`). Null `cost_usd` when none was priced; an
/// empty list for a session that ran on the node's own machine.
pub fn session_runtime(store: &Store, session_id: &str) -> Value {
    let now = crate::store::now_ms();
    let rows = store.session_sandboxes(session_id).unwrap_or_default();
    let cost = rows.iter().filter_map(|r| r.cost(now)).reduce(|a, b| a + b);
    let hours: f64 = rows
        .iter()
        .map(|r| r.hours_between(i64::MIN, None, now))
        .sum();
    json!({
        "cost_usd": cost,
        "hours": hours,
        "running": rows.iter().any(|r| r.ended_ms.is_none()),
        "sandboxes": rows,
    })
}

/// One line of the infrastructure report: sandboxes grouped by day,
/// channel, session or size, inside a window.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct InfraTotal {
    pub provider: String,
    pub channel: Option<String>,
    /// `YYYY-MM-DD` in the asked offset, when grouped by day.
    pub day: Option<String>,
    pub session_id: Option<String>,
    pub size: Option<String>,
    pub kind: Option<String>,
    pub sandboxes: i64,
    pub hours: f64,
    /// Null when no sandbox in the group was priced.
    pub cost_usd: Option<f64>,
    pub unpriced: i64,
}

/// How [`infra_usage`] groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfraGroup {
    Day,
    Channel,
    Session,
    Size,
}

impl InfraGroup {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "day" => Self::Day,
            "channel" => Self::Channel,
            "session" => Self::Session,
            "size" => Self::Size,
            _ => return None,
        })
    }
}

/// The sandboxes alive in `[since_ms, until_ms)`, grouped, each priced for
/// its part of the window; a day group splits a sandbox across the days it
/// ran on, at `tz_offset_minutes` east of UTC.
pub fn infra_usage(
    store: &Store,
    channel: Option<&str>,
    since_ms: i64,
    until_ms: Option<i64>,
    group: InfraGroup,
    tz_offset_minutes: Option<i64>,
    now_ms: i64,
) -> Result<Vec<InfraTotal>, crate::store::StoreError> {
    const DAY_MS: i64 = 86_400_000;
    // The caller's day boundary, or else the one the ceiling's "today" uses.
    let tz_offset_minutes =
        tz_offset_minutes.unwrap_or_else(crate::corpus::promote::chrono_free::offset_minutes);
    let rows = store.sandboxes_between(since_ms, until_ms, channel)?;
    let offset = tz_offset_minutes * 60_000;
    let mut out: std::collections::BTreeMap<(String, String), InfraTotal> = Default::default();
    let mut add = |key: String,
                   base: InfraTotal,
                   r: &crate::store::SandboxUsage,
                   from: i64,
                   to: Option<i64>| {
        let hours = r.hours_between(from, to, now_ms);
        if hours <= 0.0 {
            return;
        }
        let t = out.entry((r.provider.clone(), key)).or_insert(base);
        t.sandboxes += 1;
        t.hours += hours;
        match r.cost_between(from, to, now_ms) {
            Some(c) => *t.cost_usd.get_or_insert(0.0) += c,
            None => t.unpriced += 1,
        }
    };
    for r in &rows {
        let base = InfraTotal {
            provider: r.provider.clone(),
            ..Default::default()
        };
        match group {
            InfraGroup::Channel => {
                let key = r.channel.clone().unwrap_or_default();
                add(
                    key,
                    InfraTotal {
                        channel: r.channel.clone(),
                        ..base
                    },
                    r,
                    since_ms,
                    until_ms,
                );
            }
            InfraGroup::Session => {
                let key = r
                    .session_id
                    .clone()
                    .unwrap_or_else(|| format!("{}:", r.kind));
                let t = InfraTotal {
                    channel: r.channel.clone(),
                    session_id: r.session_id.clone(),
                    kind: Some(r.kind.clone()),
                    size: r.size.clone(),
                    ..base
                };
                add(key, t, r, since_ms, until_ms);
            }
            InfraGroup::Size => {
                let key = r.size.clone().unwrap_or_default();
                add(
                    key,
                    InfraTotal {
                        size: r.size.clone(),
                        ..base
                    },
                    r,
                    since_ms,
                    until_ms,
                );
            }
            InfraGroup::Day => {
                let start = r.started_ms.max(since_ms);
                let end = r
                    .ended_ms
                    .unwrap_or(now_ms)
                    .min(until_ms.unwrap_or(i64::MAX));
                let mut day = (start + offset).div_euclid(DAY_MS);
                while day * DAY_MS - offset < end {
                    let from = (day * DAY_MS - offset).max(start);
                    let to = ((day + 1) * DAY_MS - offset).min(end);
                    let name = crate::corpus::promote::chrono_free::local_date_at(
                        day * DAY_MS - offset,
                        tz_offset_minutes,
                    );
                    let t = InfraTotal {
                        day: Some(name.clone()),
                        channel: r.channel.clone(),
                        ..base.clone()
                    };
                    add(
                        format!("{name}\u{0}{}", r.channel.clone().unwrap_or_default()),
                        t,
                        r,
                        from,
                        Some(to),
                    );
                    day += 1;
                }
            }
        }
    }
    let mut totals: Vec<InfraTotal> = out.into_values().collect();
    if group == InfraGroup::Day {
        totals.sort_by(|a, b| b.day.cmp(&a.day).then(a.channel.cmp(&b.channel)));
    } else {
        totals.sort_by(|a, b| {
            b.cost_usd
                .unwrap_or(0.0)
                .total_cmp(&a.cost_usd.unwrap_or(0.0))
                .then(b.hours.total_cmp(&a.hours))
        });
    }
    Ok(totals)
}

// ---- two usage sources, one ledger ---------------------------------------
//
// The gateway counts every model call on the wire and that count is what the
// budget and the channel ceiling are charged against. The harness reports its
// own usage per turn — Claude Code's `result.usage`, tracon-agent's
// `usage_update` — and a harness that reports nothing may report zero rather
// than "unknown". So the two are recorded side by side and compared, and the harness
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
    /// The session ended under the turn (stopped, killed, the harness gone):
    /// the harness never reported, so the gateway's count is the charge.
    pub const INTERRUPTED: &str = "interrupted";
    /// The harness reports nothing for this kind of turn (Claude Code's
    /// `/compact`), so there is no second count to disagree with: the
    /// gateway's count is the charge.
    pub const UNREPORTED: &str = "unreported";
}

/// How far the two sources may differ before it is worth telling the operator.
/// Both leave cache reads out, so what is left at the edges is a retried
/// request the harness folded into one turn, or a call one side did not see;
/// the tolerance is proportional, with a floor for small turns.
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
    /// Read from a provider's cache: shown, never charged.
    pub gateway_cache_read: i64,
    /// `None` when the harness said nothing at all about this turn.
    pub harness_tokens: Option<i64>,
    pub harness_cost_usd: Option<f64>,
    /// What the budget is charged: never less than the gateway counted.
    pub charged: i64,
    /// The turn's requests matched one by one, when the harness reported
    /// them one by one.
    pub requests: Option<RequestMatch>,
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
        let mut detail = json!({
            "turn": self.turn,
            "state": self.state,
            "gateway": {
                "input_tokens": self.gateway_input,
                "output_tokens": self.gateway_output,
                "tokens": self.gateway_tokens,
                "requests": self.gateway_requests,
                "cache_read_tokens": self.gateway_cache_read,
            },
            "harness": {
                "tokens": self.harness_tokens,
                "cost_usd": self.harness_cost_usd,
            },
            "charged_tokens": self.charged,
        });
        if let Some(requests) = &self.requests {
            detail["requests_matched"] = json!(requests.matched);
            detail["unmatched_requests"] = json!({
                "harness_only": requests.harness_only,
                "gateway_only": requests.gateway_only,
            });
        }
        detail
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
        gateway_cache_read: 0,
        harness_tokens,
        harness_cost_usd,
        charged,
        requests: None,
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
    settle(store, session_id, harness_tokens, harness_cost_usd, None)
}

/// Close the ledger row of a turn the session ended under. Nothing from the
/// harness is coming, so there is nothing to disagree with: the gateway's
/// count is charged and the row says why it stands alone. Without this a
/// stopped turn's row stayed open and the session was charged nothing for it.
pub fn settle_interrupted_turn(
    store: &Store,
    session_id: &str,
    harness_cost_usd: Option<f64>,
) -> Reconciled {
    settle(
        store,
        session_id,
        None,
        harness_cost_usd,
        Some(usage_state::INTERRUPTED),
    )
}

/// Close the ledger row of a turn whose harness says nothing of what it
/// spent (see `TurnResult::unreported`): charged what the gateway counted,
/// as an interrupted turn is, and not told on as a harness that said zero.
pub fn settle_unreported_turn(
    store: &Store,
    session_id: &str,
    harness_cost_usd: Option<f64>,
) -> Reconciled {
    settle(
        store,
        session_id,
        None,
        harness_cost_usd,
        Some(usage_state::UNREPORTED),
    )
}

fn settle(
    store: &Store,
    session_id: &str,
    harness_tokens: Option<i64>,
    harness_cost_usd: Option<f64>,
    state: Option<&'static str>,
) -> Reconciled {
    let turn = store.current_turn(session_id).unwrap_or(0);
    let gateway = store
        .turn_gateway_counts(session_id, turn)
        .unwrap_or((0, 0, 0));
    let mut out = reconcile(turn, gateway, harness_tokens, harness_cost_usd);
    if let Some(state) = state {
        out.state = state;
    }
    out.gateway_cache_read = store.turn_gateway_cache_read(session_id, turn).unwrap_or(0);
    out.requests = store
        .turn_request_match(session_id, turn)
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, session_id, turn, "requests not matched");
            None
        });
    // Totals that agree can still hide a request one side never saw and
    // another the other side over-counted; matched by id, they cannot.
    if out.agreed() && out.requests.as_ref().is_some_and(|r| r.unmatched() > 0) {
        out.state = usage_state::MISMATCH;
    }
    if let Err(e) = store.settle_turn(
        session_id,
        turn,
        gateway,
        out.gateway_cache_read,
        harness_tokens,
        harness_cost_usd,
        out.charged,
        out.state,
        out.requests.as_ref(),
    ) {
        tracing::warn!(error = %e, session_id, turn, "turn usage not recorded");
    }
    if let Err(e) = store.rematch_earlier_turns(session_id, turn) {
        tracing::warn!(error = %e, session_id, turn, "earlier turns not matched again");
    }
    out
}

/// Keep what the harness said one upstream request cost, against the turn
/// it was made in, for `settle` to match against the gateway's row of the same
/// request.
pub fn record_harness_request(
    store: &Store,
    session_id: &str,
    request: &crate::adapter::types::RequestUsage,
) {
    let tokens = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
    if let Err(e) = store.record_harness_request(
        session_id,
        request.request_id.as_deref().filter(|id| !id.is_empty()),
        tokens(request.input_tokens),
        tokens(request.output_tokens),
        tokens(request.cached_read_tokens),
    ) {
        tracing::warn!(error = %e, session_id, "harness request usage not recorded");
    }
}

/// What the session API and the SPA show: the ledger, and the standing state
/// of the last turn that settled.
pub fn session_usage(store: &Store, session_id: &str) -> Value {
    let turns = store.turn_ledger(session_id, 50).unwrap_or_default();
    let settled = turns.iter().find(|t| t.state != "open");
    let gateway_tokens: i64 = turns.iter().map(|t| t.gateway_tokens()).sum();
    let harness_tokens: i64 = turns.iter().filter_map(|t| t.harness_tokens).sum();
    let cache_read_tokens: i64 = turns.iter().map(|t| t.gateway_cache_read).sum();
    // Only turns whose harness reported per request carry these; a session
    // with none of them says null rather than "nothing matched".
    let sum = |field: fn(&crate::store::TurnUsageRow) -> Option<i64>| {
        turns.iter().filter_map(field).reduce(|a, b| a + b)
    };
    json!({
        "state": settled.map(|t| t.state.clone()),
        "gateway_tokens": gateway_tokens,
        "harness_tokens": harness_tokens,
        "charged_tokens": turns.iter().map(|t| t.charged_tokens).sum::<i64>(),
        "cache_read_tokens": cache_read_tokens,
        "unmetered_turns": turns.iter().filter(|t| t.state == usage_state::UNMETERED).count(),
        "mismatched_turns": turns.iter().filter(|t| t.state == usage_state::MISMATCH).count(),
        "requests_matched": sum(|t| t.requests_matched),
        "requests_unmatched": sum(|t| t.requests_unmatched),
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
    /// Priced where the provider carries a price; null otherwise. Model
    /// (inference) cost only: sandboxes are `infra_cost_usd`.
    pub cost_usd: Option<f64>,
    /// What the channel's sandboxes on an elastic provider cost in the
    /// window; null when none ran there, or none was priced.
    #[serde(default)]
    pub infra_cost_usd: Option<f64>,
    #[serde(default)]
    pub sandbox_hours: f64,
    pub human_seconds: f64,
    pub agent_seconds: f64,
    pub sessions: i64,
    /// The newest limit snapshot a subscription provider (a ChatGPT/Codex
    /// plan) sent on this channel in the window; null when none did.
    #[serde(default)]
    pub codex_limits: Option<crate::store::LimitSnapshot>,
    #[serde(flatten)]
    pub workflow: crate::store::metrics::WorkflowMetrics,
}

/// What `input` and `output` tokens cost on `provider`, or `None` when the
/// provider carries no price.
pub fn price_of(cfg: &Config, provider: &str, input: i64, output: i64) -> Option<f64> {
    cfg.live()
        .providers
        .get(provider)
        .and_then(|p| p.price)
        .map(|price| price.cost(input, output))
}

/// Price each row of a usage report the way [`channel_metrics`] prices a
/// channel: by its provider's price, null when unpriced.
pub fn price_usage(cfg: &Config, rows: &mut [crate::store::UsageTotal]) {
    for r in rows {
        r.cost_usd = price_of(cfg, &r.provider, r.input_tokens, r.output_tokens);
    }
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
        if let Some(c) = price_of(cfg, provider, *i, *o) {
            *cost.get_or_insert(0.0) += c;
        }
    }
    let behind_accepted: std::collections::HashSet<&str> = accepted
        .iter()
        .flat_map(|r| {
            r.session_id
                .as_deref()
                .into_iter()
                .chain(r.review_session_id.as_deref())
        })
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
    let now = crate::store::now_ms();
    let sandboxes = store.sandboxes_between(since_ms, None, Some(channel))?;
    let infra_cost_usd = sandboxes
        .iter()
        .filter_map(|r| r.cost_between(since_ms, None, now))
        .reduce(|a, b| a + b);
    let sandbox_hours: f64 = sandboxes
        .iter()
        .map(|r| r.hours_between(since_ms, None, now))
        .sum();
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
        infra_cost_usd,
        sandbox_hours,
        human_seconds: human_perm + human_reviews,
        agent_seconds,
        sessions: sessions.len() as i64,
        codex_limits: store
            .latest_limit_snapshots(Some(channel), since_ms, None)?
            .into_iter()
            .max_by_key(|s| s.at_ms),
        workflow,
    })
}

/// Which model, which prompt, which approval, which policy version, for a
/// commit that shipped under the operator's name.
pub fn provenance(store: &Store, sha: &str) -> Result<Option<Value>, crate::store::StoreError> {
    let Some(review) = store.review_by_sha(sha)? else {
        return Ok(None);
    };
    let session = match review.session_id.as_deref() {
        Some(id) => store.get_session(id)?,
        None => None,
    };
    let review_session = review
        .review_session_id
        .as_deref()
        .and_then(|id| store.get_session(id).ok().flatten());
    let item = session
        .as_ref()
        .and_then(|s| s.work_item_id.as_deref())
        .and_then(|id| store.work_get(id).ok().flatten());
    let events = match review.session_id.as_deref() {
        Some(id) => store.events_after(id, 0, 5000)?,
        None => Vec::new(),
    };
    let prompts: Vec<Value> = events
        .iter()
        // A message the operator queued mid-turn is theirs whether it reached
        // the agent in a tool result or as the next prompt; that prompt names
        // it rather than repeating it.
        .filter(|e| {
            e.kind == "prompt_queued"
                || (e.kind == "user_prompt" && e.payload.get("queued").is_none())
        })
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

    const H: i64 = 3_600_000;

    fn sandbox(
        store: &Store,
        id: &str,
        channel: &str,
        started: i64,
        ended: Option<i64>,
        price: Option<f64>,
    ) {
        store
            .sandbox_started(&crate::store::SandboxUsage {
                sandbox_id: id.into(),
                provider: "digitalocean".into(),
                kind: "session".into(),
                session_id: Some(format!("s-{id}")),
                channel: Some(channel.into()),
                size: Some("small".into()),
                price_per_hour_usd: price,
                started_ms: started,
                ended_ms: ended,
                ..Default::default()
            })
            .unwrap();
    }

    #[test]
    fn infra_spend_counts_ended_and_running_sandboxes_against_the_limit() {
        let store = Store::open_in_memory().unwrap();
        let day = 100 * 24 * H;
        // Started yesterday: only today's hour counts.
        sandbox(&store, "a", "ch", day - H, Some(day + H), Some(1.0));
        // Still running, two hours so far.
        sandbox(&store, "b", "ch", day, None, Some(2.0));
        sandbox(&store, "c", "ch", day, Some(day + H), None);
        let now = day + 2 * H;
        let s = infra_spend_since(&store, None, day, now);
        assert_eq!(s.today_usd, 5.0);
        assert_eq!(s.state, "none");
        assert_eq!((s.running, s.unpriced), (1, 1));
        assert_eq!(
            infra_spend_since(&store, Some(10.0), day, now).state,
            "under"
        );
        assert_eq!(infra_spend_since(&store, Some(6.0), day, now).state, "near");
        let at = infra_spend_since(&store, Some(5.0), day, now);
        assert!(at.at(), "{at:?}");
        assert!(at.reason().contains("$5.00"), "{}", at.reason());
    }

    #[test]
    fn the_infra_report_splits_a_sandbox_across_the_days_it_ran() {
        let store = Store::open_in_memory().unwrap();
        let day = 100 * 24 * H;
        sandbox(&store, "a", "ch", day - H, Some(day + 2 * H), Some(1.0));
        sandbox(&store, "b", "other", day, Some(day + H), None);
        let rows =
            infra_usage(&store, None, 0, None, InfraGroup::Day, Some(0), day + 5 * H).unwrap();
        let summary: Vec<_> = rows
            .iter()
            .map(|r| {
                (
                    r.day.clone().unwrap(),
                    r.channel.clone().unwrap(),
                    r.hours,
                    r.cost_usd,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("1970-04-11".to_string(), "ch".to_string(), 2.0, Some(2.0)),
                ("1970-04-11".to_string(), "other".to_string(), 1.0, None),
                ("1970-04-10".to_string(), "ch".to_string(), 1.0, Some(1.0)),
            ]
        );
        assert_eq!(rows[1].unpriced, 1);

        let by_channel = infra_usage(
            &store,
            Some("ch"),
            0,
            None,
            InfraGroup::Channel,
            Some(0),
            day + 5 * H,
        )
        .unwrap();
        assert_eq!(by_channel.len(), 1);
        assert_eq!(by_channel[0].cost_usd, Some(3.0));
        assert_eq!(by_channel[0].sandboxes, 1);

        let runtime = session_runtime(&store, "s-a");
        assert_eq!(runtime["cost_usd"], 3.0);
        assert_eq!(runtime["running"], false);
        assert_eq!(session_runtime(&store, "s-none")["cost_usd"], Value::Null);
    }

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
