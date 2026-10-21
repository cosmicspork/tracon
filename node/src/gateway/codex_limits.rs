//! A ChatGPT/Codex subscription's limits, read off the headers of its answer.
//!
//! The Codex backend says where the plan stands on every answer: each usage
//! window's used percent, length and reset, the credit balance, and on a
//! refusal which limit was reached. None of it is documented; the reference
//! is the Codex CLI's own reader (`openai/codex`,
//! `codex-rs/codex-api/src/rate_limits.rs`), whose header names this follows:
//!
//! - `x-codex-{primary,secondary}-{used-percent,window-minutes,reset-at}`
//!   and `x-codex-limit-name` for the plan's own limit;
//! - the same suffixes under `x-<limit>-` for any other metered limit, one
//!   snapshot each, the plan's own first;
//! - `x-codex-credits-{has-credits,unlimited,balance}`;
//! - `x-codex-rate-limit-reached-type` on a refusal: why the plan refused
//!   (`rate_limit_reached`, `workspace_owner_credits_depleted`, ...), not
//!   which window.
//!
//! A value that does not parse is left out, never guessed, and never fails
//! the call it came with. The limit-shaped `x-codex-*` headers are also kept
//! raw beside the parsed fields, once per call, so a field the provider
//! renames is still on record. Anything else under `x-codex-` is not, notably
//! `x-codex-turn-state`, the server's sticky-routing token for the session.

use std::collections::BTreeMap;

use axum::http::HeaderMap;

use crate::store::{LimitCredits, LimitSnapshot, LimitWindow};

/// The plan's own limit, whose headers are `x-codex-*`.
pub const CODEX: &str = "codex";
const USED: &str = "-primary-used-percent";
/// What an `x-codex-*` header kept raw must name: a limit, a window, its
/// use or reset, or the credits.
const LIMIT_SHAPED: &[&str] = &["limit", "percent", "window", "reset", "credits"];

/// Whether `name` is an `x-codex-*` header about the plan's limits.
fn limit_shaped(name: &str) -> bool {
    name.strip_prefix("x-codex-")
        .is_some_and(|rest| LIMIT_SHAPED.iter().any(|w| rest.contains(w)))
}

/// Every limit snapshot `headers` carry, the plan's own first and the rest
/// by name. Channel, provider, time and usage row are the caller's to fill.
/// The raw headers go on the first snapshot only; the rest carry `{}`.
pub fn parse(headers: &HeaderMap) -> Vec<LimitSnapshot> {
    let raw: BTreeMap<String, String> = headers
        .iter()
        .filter(|(k, _)| limit_shaped(k.as_str()))
        .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.trim().to_string())))
        .collect();
    let raw_json = serde_json::to_string(&raw).unwrap_or_else(|_| "{}".into());
    let mut others: Vec<String> = headers
        .keys()
        .filter_map(|k| limit_id(k.as_str()))
        .filter(|id| id != CODEX)
        .collect();
    others.sort();
    others.dedup();
    let mut snaps: Vec<LimitSnapshot> = std::iter::once(CODEX.to_string())
        .chain(others)
        .filter_map(|id| snapshot(headers, &id))
        .collect();
    if let Some(first) = snaps.first_mut() {
        first.raw_json = raw_json;
    }
    snaps
}

/// `x-codex-other-primary-used-percent` → `codex_other`.
fn limit_id(header: &str) -> Option<String> {
    let prefix = header.strip_suffix(USED)?.strip_prefix("x-")?;
    (!prefix.is_empty()).then(|| prefix.replace('-', "_"))
}

fn snapshot(headers: &HeaderMap, id: &str) -> Option<LimitSnapshot> {
    let prefix = format!("x-{}", id.replace('_', "-"));
    let text = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    let window = |which: &str| {
        let used = text(&format!("{prefix}-{which}-used-percent"))?
            .parse::<f64>()
            .ok()
            .filter(|p| p.is_finite() && *p >= 0.0)?;
        let whole = |suffix: &str| {
            text(&format!("{prefix}-{which}-{suffix}"))?
                .parse::<i64>()
                .ok()
                .filter(|n| *n >= 0)
        };
        Some(LimitWindow {
            used_percent: used,
            window_minutes: whole("window-minutes"),
            resets_at: whole("reset-at"),
        })
    };
    let own = id == CODEX;
    let flag = |name: &str| match text(name)?.to_ascii_lowercase().as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    };
    let credits = own
        .then(|| LimitCredits {
            has_credits: flag("x-codex-credits-has-credits"),
            unlimited: flag("x-codex-credits-unlimited"),
            balance: text("x-codex-credits-balance"),
        })
        .filter(|c| *c != LimitCredits::default());
    let snap = LimitSnapshot {
        limit_id: id.to_string(),
        primary: window("primary"),
        secondary: window("secondary"),
        credits,
        limit_name: text(&format!("{prefix}-limit-name")),
        reached_type: own
            .then(|| text("x-codex-rate-limit-reached-type"))
            .flatten(),
        raw_json: "{}".to_string(),
        ..LimitSnapshot::default()
    };
    let said_something = snap.primary.is_some()
        || snap.secondary.is_some()
        || snap.credits.is_some()
        || snap.limit_name.is_some()
        || snap.reached_type.is_some();
    said_something.then_some(snap)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn a_full_set_of_headers_is_one_snapshot_of_both_windows_and_the_credits() {
        let snaps = parse(&headers(&[
            ("x-codex-primary-used-percent", "42.5"),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-primary-reset-at", "1792600000"),
            ("x-codex-secondary-used-percent", "7"),
            ("x-codex-secondary-window-minutes", "10080"),
            ("x-codex-secondary-reset-at", "1793000000"),
            ("x-codex-limit-name", "plus"),
            ("x-codex-credits-has-credits", "true"),
            ("x-codex-credits-unlimited", "False"),
            ("x-codex-credits-balance", "12.50"),
            ("x-codex-primary-over-secondary-limit-percent", "3"),
            ("content-type", "text/event-stream"),
        ]));
        assert_eq!(snaps.len(), 1);
        let s = &snaps[0];
        assert_eq!(s.limit_id, "codex");
        assert_eq!(
            s.primary,
            Some(LimitWindow {
                used_percent: 42.5,
                window_minutes: Some(300),
                resets_at: Some(1_792_600_000)
            })
        );
        assert_eq!(s.secondary.as_ref().unwrap().window_minutes, Some(10_080));
        assert_eq!(
            s.credits,
            Some(LimitCredits {
                has_credits: Some(true),
                unlimited: Some(false),
                balance: Some("12.50".into())
            })
        );
        assert_eq!(s.limit_name.as_deref(), Some("plus"));
        assert_eq!(s.reached_type, None);
        // A header this reader does not know is kept raw; others are not.
        let raw: serde_json::Value = serde_json::from_str(&s.raw_json).unwrap();
        assert_eq!(raw["x-codex-primary-over-secondary-limit-percent"], "3");
        assert!(raw.get("content-type").is_none());
    }

    #[test]
    fn a_primary_window_alone_is_a_snapshot_without_the_rest() {
        let snaps = parse(&headers(&[("x-codex-primary-used-percent", "90")]));
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].primary.as_ref().unwrap().used_percent, 90.0);
        assert_eq!(snaps[0].primary.as_ref().unwrap().window_minutes, None);
        assert!(snaps[0].secondary.is_none() && snaps[0].credits.is_none());
    }

    #[test]
    fn credits_alone_are_a_snapshot_without_windows() {
        let snaps = parse(&headers(&[("x-codex-credits-balance", "0")]));
        assert_eq!(snaps.len(), 1);
        assert!(snaps[0].primary.is_none());
        assert_eq!(
            snaps[0].credits.as_ref().unwrap().balance.as_deref(),
            Some("0")
        );
    }

    #[test]
    fn an_answer_without_codex_headers_has_no_snapshot() {
        assert!(parse(&headers(&[("x-request-id", "abc"), ("retry-after", "3")])).is_empty());
    }

    #[test]
    fn garbage_values_are_left_out_rather_than_guessed() {
        let snaps = parse(&headers(&[
            ("x-codex-primary-used-percent", "lots"),
            ("x-codex-secondary-used-percent", "NaN"),
            ("x-codex-credits-has-credits", "maybe"),
        ]));
        // Nothing readable is nothing to record, though the raw map has it all.
        assert!(snaps.is_empty());
        let snaps = parse(&headers(&[
            ("x-codex-primary-used-percent", "12"),
            ("x-codex-primary-window-minutes", "-5"),
            ("x-codex-primary-reset-at", "tomorrow"),
            ("x-codex-secondary-used-percent", "-1"),
        ]));
        let p = snaps[0].primary.as_ref().unwrap();
        assert_eq!((p.window_minutes, p.resets_at), (None, None));
        assert!(snaps[0].secondary.is_none());
    }

    #[test]
    fn a_refusal_names_the_limit_it_reached_and_other_limits_follow_the_plans_own() {
        let snaps = parse(&headers(&[
            ("x-codex-rate-limit-reached-type", "rate_limit_reached"),
            ("x-codex-primary-used-percent", "100"),
            ("x-codex-other-primary-used-percent", "55"),
            ("x-codex-other-limit-name", "gpt-5-codex"),
        ]));
        let ids: Vec<_> = snaps.iter().map(|s| s.limit_id.as_str()).collect();
        assert_eq!(ids, ["codex", "codex_other"]);
        assert_eq!(snaps[0].reached_type.as_deref(), Some("rate_limit_reached"));
        assert_eq!(
            snaps[1].reached_type, None,
            "the reached type is the plan's own"
        );
        assert_eq!(snaps[1].limit_name.as_deref(), Some("gpt-5-codex"));
        assert_eq!(snaps[1].primary.as_ref().unwrap().used_percent, 55.0);
        // The call's raw headers are kept once, on its first snapshot.
        let raw: serde_json::Value = serde_json::from_str(&snaps[0].raw_json).unwrap();
        assert_eq!(raw["x-codex-other-primary-used-percent"], "55");
        assert_eq!(snaps[1].raw_json, "{}");
    }

    #[test]
    fn the_sticky_routing_token_is_not_kept_raw() {
        let snaps = parse(&headers(&[
            ("x-codex-primary-used-percent", "20"),
            ("x-codex-turn-state", "opaque-routing-token"),
        ]));
        let raw: serde_json::Value = serde_json::from_str(&snaps[0].raw_json).unwrap();
        assert_eq!(raw["x-codex-primary-used-percent"], "20");
        assert!(raw.get("x-codex-turn-state").is_none(), "{raw}");
        assert!(!snaps[0].raw_json.contains("opaque-routing-token"));
    }
}
