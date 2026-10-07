//! What a session does when its provider will not serve it any more.
//!
//! A refused model call is one of four things, and only one of them is
//! exhaustion. Throttling (a per-minute limit) and an outage clear on their
//! own, and the harness already retries both with backoff. An auth failure
//! does not clear by waiting. Exhaustion — a spent quota, a subscription's
//! usage window, an empty credit balance — is the one where the work cannot
//! go on until either time passes or another provider takes over, so it is
//! the one a channel chooses a policy for:
//!
//! - `pause` (the default): fence the session at its next safe boundary and
//!   resume it when the provider says its limit resets.
//! - `fallback`: carry the work on to a named model as a continuation from
//!   the boundary; if that is not possible, or the fallback is exhausted too,
//!   hold the session for the operator.
//! - `fallback_then_wait`: the same, but when falling back is not possible
//!   the session waits for the reset like `pause`.
//!
//! The reset time is only ever one the provider sent. A provider that gave
//! none leaves the session held for the operator: a guessed timer would wake
//! it into the same refusal, or leave it asleep long after the limit lifted.

use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The binding key a channel sets its policy under.
pub const BINDING_KEY: &str = "exhaustion";

/// What kind of refusal an upstream answer is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// The quota, usage window or balance behind the credential is spent.
    Exhausted,
    /// A short-window rate limit; the harness's own retries clear it.
    Throttled,
    /// The provider refused the credential.
    Auth,
    /// The provider, or the route to it, is failing.
    Outage,
    /// Anything else: a request the provider found wrong.
    Rejected,
}

impl Cause {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exhausted => "exhausted",
            Self::Throttled => "throttled",
            Self::Auth => "auth",
            Self::Outage => "outage",
            Self::Rejected => "rejected",
        }
    }
}

/// Phrases providers use for a spent quota or balance, as distinct from a
/// per-minute limit. Matched against the error's type, code and message,
/// lowercased. "rate limit" alone is deliberately absent: it is how
/// throttling is worded too.
const EXHAUSTED: &[&str] = &[
    "insufficient_quota",
    "quota",
    "usage limit",
    "usage_limit",
    "credit balance",
    "billing",
    "spend limit",
    "spending limit",
];

/// Classify one refused upstream answer.
///
/// Anthropic's subscription limits answer 429 with
/// `anthropic-ratelimit-unified-status: rejected`; API keys say the balance
/// or quota is spent in the body, on a 429 (OpenAI's `insufficient_quota`)
/// or a 400 (Anthropic's credit balance). A bare 429 is throttling: that
/// includes the misleading `rate_limit_error` "Error" an unshaped
/// subscription call gets, which says nothing about a spent window.
pub fn classify(status: u16, headers: &HeaderMap, body: &[u8]) -> Cause {
    let unified_rejected = headers
        .get("anthropic-ratelimit-unified-status")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("rejected"));
    let says_exhausted = || {
        let text = error_text(body).to_ascii_lowercase();
        EXHAUSTED.iter().any(|p| text.contains(p))
    };
    match status {
        402 => Cause::Exhausted,
        429 if unified_rejected || says_exhausted() => Cause::Exhausted,
        429 => Cause::Throttled,
        401 | 403 if says_exhausted() => Cause::Exhausted,
        401 | 403 => Cause::Auth,
        400 if says_exhausted() => Cause::Exhausted,
        500..=599 => Cause::Outage,
        _ => Cause::Rejected,
    }
}

/// The error's type, code and message, whichever of the two common shapes
/// the body is in; the raw body when it is not JSON.
fn error_text(body: &[u8]) -> String {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return String::from_utf8_lossy(body).into_owned();
    };
    let e = &v["error"];
    [
        &e["type"],
        &e["code"],
        &e["message"],
        &v["type"],
        &v["code"],
        &v["message"],
        e,
    ]
    .iter()
    .filter_map(|v| v.as_str())
    .collect::<Vec<_>>()
    .join(" ")
}

/// When the provider says the limit lifts, in epoch milliseconds, or `None`
/// when it did not say in a form this reads. The latest of the times given
/// wins: the session needs every limit that refused it to have lifted.
pub fn reset_ms(headers: &HeaderMap, now_ms: i64) -> Option<i64> {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let mut at: Vec<i64> = Vec::new();
    // Delta seconds. An HTTP-date is not read; it is rare from these APIs.
    if let Some(secs) = header("retry-after").and_then(|v| v.trim().parse::<f64>().ok()) {
        if secs.is_finite() && secs >= 0.0 {
            at.push(now_ms + (secs * 1000.0) as i64);
        }
    }
    // Anthropic's subscription window: epoch seconds.
    if let Some(secs) =
        header("anthropic-ratelimit-unified-reset").and_then(|v| v.trim().parse::<i64>().ok())
    {
        at.push(secs.saturating_mul(1000));
    }
    // Anthropic's API limits: RFC 3339.
    for name in [
        "anthropic-ratelimit-requests-reset",
        "anthropic-ratelimit-tokens-reset",
        "anthropic-ratelimit-input-tokens-reset",
        "anthropic-ratelimit-output-tokens-reset",
    ] {
        if let Some(ms) = header(name).and_then(rfc3339_ms) {
            at.push(ms);
        }
    }
    // OpenAI's: a Go-style duration, `6m0s`, `1.5s`, `20ms`.
    for name in ["x-ratelimit-reset-requests", "x-ratelimit-reset-tokens"] {
        if let Some(ms) = header(name).and_then(duration_ms) {
            at.push(now_ms + ms);
        }
    }
    at.into_iter().max()
}

/// `2026-10-07T12:00:00Z`, with optional fractional seconds and a `Z` or a
/// `±HH:MM` offset.
fn rfc3339_ms(text: &str) -> Option<i64> {
    let text = text.trim();
    let (date, time) = text.split_once(['T', 't', ' '])?;
    let mut d = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let (clock, offset_min) = if let Some(clock) = time.strip_suffix(['Z', 'z']) {
        (clock, 0)
    } else {
        let at = time.rfind(['+', '-'])?;
        let (clock, off) = time.split_at(at);
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let (h, mi) = off[1..].split_once(':')?;
        (
            clock,
            sign * (h.parse::<i64>().ok()? * 60 + mi.parse::<i64>().ok()?),
        )
    };
    let mut c = clock.splitn(3, ':');
    let (h, mi, s) = (c.next()?, c.next()?, c.next()?);
    let (h, mi) = (h.parse::<i64>().ok()?, mi.parse::<i64>().ok()?);
    let secs: f64 = s.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || h > 23 || mi > 59 || secs >= 61.0 {
        return None;
    }
    let days = days_from_civil(y, m, day);
    let whole = days * 86_400 + h * 3600 + mi * 60 - offset_min * 60;
    Some(whole * 1000 + (secs * 1000.0) as i64)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// An epoch-milliseconds instant as `2026-10-07 12:00 UTC`, for a sentence.
pub fn utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60
    )
}

/// `1h2m3.5s`, `20ms`, `6m0s`.
fn duration_ms(text: &str) -> Option<i64> {
    let mut rest = text.trim();
    if rest.is_empty() {
        return None;
    }
    let mut total = 0.0f64;
    while !rest.is_empty() {
        let end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let n: f64 = rest[..end].parse().ok()?;
        rest = &rest[end..];
        let (unit, len) = if rest.starts_with("ms") {
            (1.0, 2)
        } else if rest.starts_with('h') {
            (3_600_000.0, 1)
        } else if rest.starts_with('m') {
            (60_000.0, 1)
        } else if rest.starts_with('s') {
            (1000.0, 1)
        } else {
            return None;
        };
        total += n * unit;
        rest = &rest[len..];
    }
    Some(total as i64)
}

/// What a channel, or one run, chose to happen on exhaustion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    #[default]
    Pause,
    Fallback,
    FallbackThenWait,
}

impl Policy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Fallback => "fallback",
            Self::FallbackThenWait => "fallback_then_wait",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "pause" => Some(Self::Pause),
            "fallback" => Some(Self::Fallback),
            "fallback_then_wait" => Some(Self::FallbackThenWait),
            _ => None,
        }
    }

    /// Whether, when falling back is not possible, the session waits for the
    /// provider's reset rather than for the operator.
    pub fn waits(self) -> bool {
        matches!(self, Self::Pause | Self::FallbackThenWait)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Choice {
    #[serde(default)]
    pub policy: Policy,
    /// The `provider/model` to carry the work on to. Required by the two
    /// fallback policies; ignored by `pause`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
}

impl Choice {
    fn check(self) -> Result<Self, String> {
        let fallback = self
            .fallback
            .map(|f| f.trim().to_string())
            .filter(|f| !f.is_empty());
        if self.policy != Policy::Pause && fallback.is_none() {
            return Err(format!(
                "the `{}` exhaustion policy needs a fallback model",
                self.policy.as_str()
            ));
        }
        Ok(Self {
            policy: self.policy,
            fallback,
        })
    }
}

/// The run's own choice wins over the channel's; a channel with none pauses.
pub fn resolve(run: Option<&Choice>, bindings: &Value) -> Result<Choice, String> {
    if let Some(choice) = run {
        return choice.clone().check();
    }
    let bound = &bindings[BINDING_KEY];
    if bound.is_null() {
        return Ok(Choice::default());
    }
    let choice: Choice = serde_json::from_value(bound.clone())
        .map_err(|e| format!("the channel's exhaustion policy does not parse: {e}"))?;
    choice.check()
}

/// What the node decided for one exhaustion, before the supervisor fences the
/// session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub policy: Policy,
    /// `waiting` (resumes at `next_wake_ms`), `falling_back` (continues on
    /// `fallback` from the boundary) or `held` (for the operator).
    pub outcome: &'static str,
    pub fallback: Option<String>,
    pub next_wake_ms: Option<i64>,
    /// Why it is held, or why it is not falling back, in a sentence.
    pub note: Option<String>,
}

pub const WAITING: &str = "waiting";
pub const FALLING_BACK: &str = "falling_back";
pub const HELD: &str = "held";
pub const RESUMED: &str = "resumed";
pub const CONTINUED: &str = "continued";
pub const OPERATOR: &str = "operator";
pub const ABANDONED: &str = "abandoned";

/// Decide what happens. `fallback_usable` is `Err(why)` when the fallback
/// cannot take the work (not bound, no credential, the model in use already).
pub fn decide(
    choice: &Choice,
    reset_ms: Option<i64>,
    fallback_usable: Result<(), String>,
) -> Decision {
    let wait = |note: Option<String>| match reset_ms {
        Some(at) => Decision {
            policy: choice.policy,
            outcome: WAITING,
            fallback: choice.fallback.clone(),
            next_wake_ms: Some(at),
            note,
        },
        None => Decision {
            policy: choice.policy,
            outcome: HELD,
            fallback: choice.fallback.clone(),
            next_wake_ms: None,
            note: Some(match note {
                Some(n) => format!("{n} The provider gave no reset time."),
                None => "The provider gave no reset time.".into(),
            }),
        },
    };
    if choice.policy == Policy::Pause {
        return wait(None);
    }
    match fallback_usable {
        Ok(()) => Decision {
            policy: choice.policy,
            outcome: FALLING_BACK,
            fallback: choice.fallback.clone(),
            next_wake_ms: None,
            note: None,
        },
        Err(why) if choice.policy.waits() => wait(Some(format!("Not falling back: {why}."))),
        Err(why) => Decision {
            policy: choice.policy,
            outcome: HELD,
            fallback: choice.fallback.clone(),
            next_wake_ms: None,
            note: Some(format!("Not falling back: {why}.")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderName, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn exhaustion_is_told_apart_from_throttling_auth_and_outage() {
        let none = HeaderMap::new();
        let throttle = br#"{"type":"error","error":{"type":"rate_limit_error","message":"Number of request tokens has exceeded your per-minute rate limit"}}"#;
        assert_eq!(classify(429, &none, throttle), Cause::Throttled);
        // The unshaped-subscription trap reads as throttling, not a spent window.
        let trap = br#"{"type":"error","error":{"type":"rate_limit_error","message":"Error"}}"#;
        assert_eq!(classify(429, &none, trap), Cause::Throttled);
        let window = headers(&[("anthropic-ratelimit-unified-status", "rejected")]);
        assert_eq!(classify(429, &window, trap), Cause::Exhausted);
        let quota = br#"{"error":{"message":"You exceeded your current quota, please check your plan and billing details.","type":"insufficient_quota","code":"insufficient_quota"}}"#;
        assert_eq!(classify(429, &none, quota), Cause::Exhausted);
        let credit = br#"{"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API."}}"#;
        assert_eq!(classify(400, &none, credit), Cause::Exhausted);
        assert_eq!(classify(402, &none, b""), Cause::Exhausted);
        assert_eq!(classify(401, &none, b"{}"), Cause::Auth);
        assert_eq!(classify(403, &none, b"forbidden"), Cause::Auth);
        assert_eq!(classify(529, &none, b"overloaded"), Cause::Outage);
        assert_eq!(classify(502, &none, b"<html>"), Cause::Outage);
        assert_eq!(classify(400, &none, b"{}"), Cause::Rejected);
    }

    #[test]
    fn the_reset_is_only_ever_one_the_provider_sent() {
        let now = 1_000_000;
        assert_eq!(reset_ms(&HeaderMap::new(), now), None);
        assert_eq!(
            reset_ms(&headers(&[("retry-after", "30")]), now),
            Some(now + 30_000)
        );
        assert_eq!(
            reset_ms(
                &headers(&[("retry-after", "Wed, 21 Oct 2015 07:28:00 GMT")]),
                now
            ),
            None
        );
        assert_eq!(
            reset_ms(
                &headers(&[("anthropic-ratelimit-unified-reset", "1791374400")]),
                now
            ),
            Some(1_791_374_400_000)
        );
        assert_eq!(
            reset_ms(
                &headers(&[("anthropic-ratelimit-tokens-reset", "2026-10-07T12:00:00Z")]),
                now
            ),
            Some(1_791_374_400_000)
        );
        assert_eq!(
            reset_ms(
                &headers(&[(
                    "anthropic-ratelimit-requests-reset",
                    "2026-10-07T14:00:00.5+02:00"
                )]),
                now
            ),
            Some(1_791_374_400_500)
        );
        // The latest wins: every limit that refused has to have lifted.
        assert_eq!(
            reset_ms(
                &headers(&[
                    ("x-ratelimit-reset-requests", "1s"),
                    ("x-ratelimit-reset-tokens", "6m0.5s"),
                ]),
                now
            ),
            Some(now + 360_500)
        );
        assert_eq!(
            reset_ms(&headers(&[("x-ratelimit-reset-tokens", "soon")]), now),
            None
        );
    }

    #[test]
    fn an_instant_reads_as_a_utc_time() {
        assert_eq!(utc(1_791_374_400_000), "2026-10-07 12:00 UTC");
        assert_eq!(utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc(951_782_400_000), "2000-02-29 00:00 UTC");
    }

    #[test]
    fn the_run_overrides_the_channel_and_fallbacks_need_a_model() {
        let channel =
            serde_json::json!({ "exhaustion": { "policy": "fallback", "fallback": "b/m" } });
        assert_eq!(
            resolve(None, &channel).unwrap(),
            Choice {
                policy: Policy::Fallback,
                fallback: Some("b/m".into())
            }
        );
        let run = Choice::default();
        assert_eq!(resolve(Some(&run), &channel).unwrap().policy, Policy::Pause);
        assert_eq!(
            resolve(None, &serde_json::json!({})).unwrap(),
            Choice::default()
        );
        assert!(resolve(
            None,
            &serde_json::json!({ "exhaustion": { "policy": "fallback_then_wait" } })
        )
        .is_err());
        assert!(resolve(
            None,
            &serde_json::json!({ "exhaustion": { "policy": "later" } })
        )
        .is_err());
    }

    #[test]
    fn a_fallback_that_cannot_run_holds_or_waits_by_policy() {
        let fb = |policy| Choice {
            policy,
            fallback: Some("b/m".into()),
        };
        let d = decide(&Choice::default(), Some(5), Ok(()));
        assert_eq!((d.outcome, d.next_wake_ms), (WAITING, Some(5)));
        let d = decide(&Choice::default(), None, Ok(()));
        assert_eq!((d.outcome, d.next_wake_ms), (HELD, None));
        let d = decide(&fb(Policy::Fallback), Some(5), Ok(()));
        assert_eq!(d.outcome, FALLING_BACK);
        let d = decide(&fb(Policy::Fallback), Some(5), Err("no".into()));
        assert_eq!((d.outcome, d.next_wake_ms), (HELD, None));
        let d = decide(&fb(Policy::FallbackThenWait), Some(5), Err("no".into()));
        assert_eq!((d.outcome, d.next_wake_ms), (WAITING, Some(5)));
        assert!(d.note.unwrap().contains("Not falling back: no."));
        let d = decide(&fb(Policy::FallbackThenWait), None, Err("no".into()));
        assert_eq!(d.outcome, HELD);
    }
}
