//! What a subscription provider said about its own limits, per gateway call.
//!
//! A ChatGPT/Codex subscription is metered in usage windows and credits, not
//! per-token prices, and says where it stands on every answer it sends. The
//! gateway keeps each snapshot beside the call it came with, so the Usage
//! page can show how much of the plan is used before the plan says no.

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::{Result, Store};

/// One usage window: how much of it is used, how long it is, when it resets.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LimitWindow {
    pub used_percent: f64,
    pub window_minutes: Option<i64>,
    /// Unix seconds.
    pub resets_at: Option<i64>,
}

/// What the subscription says about its credits. The balance is kept as the
/// provider wrote it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LimitCredits {
    pub has_credits: Option<bool>,
    pub unlimited: Option<bool>,
    pub balance: Option<String>,
}

/// One limit's state as one gateway call saw it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LimitSnapshot {
    /// The `model_usage` row of the call it came with.
    pub usage_id: Option<i64>,
    pub channel: String,
    pub provider: String,
    /// `codex` for the plan's own limit; another metered limit by its name.
    pub limit_id: String,
    #[serde(rename = "observed_ms")]
    pub at_ms: i64,
    pub primary: Option<LimitWindow>,
    pub secondary: Option<LimitWindow>,
    pub credits: Option<LimitCredits>,
    pub limit_name: Option<String>,
    /// Set on a refusal: why the plan refused, as Codex names the reason
    /// (`rate_limit_reached`, `workspace_owner_credits_depleted`, ...).
    pub reached_type: Option<String>,
    /// The call's limit-shaped `x-codex-*` headers as they arrived,
    /// lower-cased, so a field the provider renames is still on record. Kept
    /// once per call, on its first snapshot; the others carry `{}`.
    #[serde(skip)]
    pub raw_json: String,
}

/// The newest snapshot of the plan's own limit in a window, on the
/// `(channel, provider, limit_id, at_ms)` index.
const NEWEST_OWN: &str = "SELECT * FROM provider_limit_snapshot
    WHERE channel = ?1 AND provider = ?2 AND limit_id = 'codex'
      AND at_ms >= ?3 AND (?4 IS NULL OR at_ms < ?4)
    ORDER BY at_ms DESC, id DESC LIMIT 1";
/// The newest snapshot of any limit in a window, on the
/// `(channel, provider, at_ms)` index.
const NEWEST_ANY: &str = "SELECT * FROM provider_limit_snapshot
    WHERE channel = ?1 AND provider = ?2
      AND at_ms >= ?3 AND (?4 IS NULL OR at_ms < ?4)
    ORDER BY at_ms DESC, id DESC LIMIT 1";

/// Every provider that reported on one channel in a window, on the
/// `(channel, provider, at_ms)` index.
const PAIRS_ON_CHANNEL: &str = "SELECT DISTINCT channel, provider FROM provider_limit_snapshot
    WHERE channel = ?3 AND at_ms >= ?1 AND (?2 IS NULL OR at_ms < ?2)
    ORDER BY channel, provider";
/// Every channel and provider that reported in a window.
const PAIRS_ALL: &str = "SELECT DISTINCT channel, provider FROM provider_limit_snapshot
    WHERE at_ms >= ?1 AND (?2 IS NULL OR at_ms < ?2)
    ORDER BY channel, provider";

impl Store {
    pub fn record_limit_snapshot(&self, s: &LimitSnapshot) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let window = |w: &Option<LimitWindow>| {
            (
                w.as_ref().map(|w| w.used_percent),
                w.as_ref().and_then(|w| w.window_minutes),
                w.as_ref().and_then(|w| w.resets_at),
            )
        };
        let (p_used, p_minutes, p_reset) = window(&s.primary);
        let (s_used, s_minutes, s_reset) = window(&s.secondary);
        let credits = s.credits.clone().unwrap_or_default();
        conn.execute(
            "INSERT INTO provider_limit_snapshot (usage_id, channel, provider, limit_id, at_ms,
                primary_used_percent, primary_window_minutes, primary_resets_at,
                secondary_used_percent, secondary_window_minutes, secondary_resets_at,
                credits_has, credits_unlimited, credits_balance, limit_name, reached_type, raw_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                s.usage_id,
                s.channel,
                s.provider,
                s.limit_id,
                s.at_ms,
                p_used,
                p_minutes,
                p_reset,
                s_used,
                s_minutes,
                s_reset,
                credits.has_credits,
                credits.unlimited,
                credits.balance,
                s.limit_name,
                s.reached_type,
                s.raw_json,
            ],
        )?;
        Ok(())
    }

    /// The newest snapshot of the plan's own limit (`codex`, else any) a
    /// channel's provider reported in `[since_ms, until_ms)`, or since
    /// `since_ms` when there is no upper bound.
    ///
    /// The plan's own limit is looked up first, on its own index, so neither
    /// read sorts the window; any other limit is read only when the plan's
    /// own has nothing in it.
    pub fn latest_limit_snapshot(
        &self,
        channel: &str,
        provider: &str,
        since_ms: i64,
        until_ms: Option<i64>,
    ) -> Result<Option<LimitSnapshot>> {
        let conn = self.conn.lock().unwrap();
        let own = conn
            .query_row(
                NEWEST_OWN,
                params![channel, provider, since_ms, until_ms],
                snapshot_from_row,
            )
            .optional()?;
        if own.is_some() {
            return Ok(own);
        }
        conn.query_row(
            NEWEST_ANY,
            params![channel, provider, since_ms, until_ms],
            snapshot_from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// [`Store::latest_limit_snapshot`] for every channel and provider that
    /// reported one in the window, on one channel or all, in a fixed order.
    pub fn latest_limit_snapshots(
        &self,
        channel: Option<&str>,
        since_ms: i64,
        until_ms: Option<i64>,
    ) -> Result<Vec<LimitSnapshot>> {
        let pairs: Vec<(String, String)> = {
            let conn = self.conn.lock().unwrap();
            let to_pair = |r: &rusqlite::Row<'_>| Ok((r.get(0)?, r.get(1)?));
            // Split, so a channel given is an equality the index can search
            // on rather than a filter applied to the table's whole history.
            let rows: rusqlite::Result<Vec<_>> = match channel {
                Some(channel) => conn
                    .prepare(PAIRS_ON_CHANNEL)?
                    .query_map(params![since_ms, until_ms, channel], to_pair)?
                    .collect(),
                None => conn
                    .prepare(PAIRS_ALL)?
                    .query_map(params![since_ms, until_ms], to_pair)?
                    .collect(),
            };
            rows?
        };
        let mut out = Vec::new();
        for (channel, provider) in pairs {
            out.extend(self.latest_limit_snapshot(&channel, &provider, since_ms, until_ms)?);
        }
        Ok(out)
    }
}

fn snapshot_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<LimitSnapshot> {
    let window =
        |used: &str, minutes: &str, reset: &str| -> rusqlite::Result<Option<LimitWindow>> {
            let Some(used_percent) = r.get::<_, Option<f64>>(used)? else {
                return Ok(None);
            };
            Ok(Some(LimitWindow {
                used_percent,
                window_minutes: r.get(minutes)?,
                resets_at: r.get(reset)?,
            }))
        };
    let credits = LimitCredits {
        has_credits: r.get("credits_has")?,
        unlimited: r.get("credits_unlimited")?,
        balance: r.get("credits_balance")?,
    };
    Ok(LimitSnapshot {
        usage_id: r.get("usage_id")?,
        channel: r.get("channel")?,
        provider: r.get("provider")?,
        limit_id: r.get("limit_id")?,
        at_ms: r.get("at_ms")?,
        primary: window(
            "primary_used_percent",
            "primary_window_minutes",
            "primary_resets_at",
        )?,
        secondary: window(
            "secondary_used_percent",
            "secondary_window_minutes",
            "secondary_resets_at",
        )?,
        credits: (credits != LimitCredits::default()).then_some(credits),
        limit_name: r.get("limit_name")?,
        reached_type: r.get("reached_type")?,
        raw_json: r.get("raw_json")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(provider: &str, limit_id: &str, at_ms: i64, used: f64) -> LimitSnapshot {
        LimitSnapshot {
            usage_id: None,
            channel: "work".into(),
            provider: provider.into(),
            limit_id: limit_id.into(),
            at_ms,
            primary: Some(LimitWindow {
                used_percent: used,
                window_minutes: Some(300),
                resets_at: Some(1_792_600_000),
            }),
            secondary: None,
            credits: None,
            limit_name: None,
            reached_type: None,
            raw_json: r#"{"x-codex-primary-used-percent":"1"}"#.into(),
        }
    }

    #[test]
    fn a_snapshot_reads_back_as_it_was_written_and_the_newest_of_the_plan_wins() {
        let store = Store::open_in_memory().unwrap();
        let full = LimitSnapshot {
            secondary: Some(LimitWindow {
                used_percent: 12.5,
                window_minutes: None,
                resets_at: Some(1_793_000_000),
            }),
            credits: Some(LimitCredits {
                has_credits: Some(true),
                unlimited: Some(false),
                balance: Some("9.99".into()),
            }),
            limit_name: Some("plus".into()),
            reached_type: Some("rate_limit_reached".into()),
            ..snap("codex", "codex", 2_000, 40.0)
        };
        store
            .record_limit_snapshot(&snap("codex", "codex", 1_000, 10.0))
            .unwrap();
        store.record_limit_snapshot(&full).unwrap();
        // Newer, but another limit than the plan's own.
        store
            .record_limit_snapshot(&snap("codex", "codex_other", 3_000, 99.0))
            .unwrap();
        store
            .record_limit_snapshot(&snap("codex-2", "codex", 500, 5.0))
            .unwrap();

        let back = store
            .latest_limit_snapshot("work", "codex", 0, None)
            .unwrap()
            .unwrap();
        assert_eq!(back, full);
        // A window that starts after it has nothing of the plan's own, so the
        // other limit is what there is.
        let later = store
            .latest_limit_snapshot("work", "codex", 2_500, None)
            .unwrap()
            .unwrap();
        assert_eq!(later.limit_id, "codex_other");
        assert!(store
            .latest_limit_snapshot("work", "codex", 4_000, None)
            .unwrap()
            .is_none());
        assert!(store
            .latest_limit_snapshot("personal", "codex", 0, None)
            .unwrap()
            .is_none());

        let all = store.latest_limit_snapshots(None, 0, None).unwrap();
        let keys: Vec<_> = all.iter().map(|s| (s.provider.as_str(), s.at_ms)).collect();
        assert_eq!(keys, [("codex", 2_000), ("codex-2", 500)]);
        assert!(store
            .latest_limit_snapshots(Some("personal"), 0, None)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_bounded_window_does_not_show_a_snapshot_taken_after_it() {
        let store = Store::open_in_memory().unwrap();
        for (at, used) in [(1_000, 10.0), (2_000, 20.0), (3_000, 30.0)] {
            store
                .record_limit_snapshot(&snap("codex", "codex", at, used))
                .unwrap();
        }
        let within = store
            .latest_limit_snapshot("work", "codex", 0, Some(3_000))
            .unwrap()
            .unwrap();
        assert_eq!(within.at_ms, 2_000, "the bound is exclusive");
        assert!(store
            .latest_limit_snapshot("work", "codex", 0, Some(1_000))
            .unwrap()
            .is_none());
        let all = store.latest_limit_snapshots(None, 0, Some(2_500)).unwrap();
        assert_eq!(all.iter().map(|s| s.at_ms).collect::<Vec<_>>(), [2_000]);
        assert!(store
            .latest_limit_snapshots(None, 3_500, Some(9_000))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn the_newest_snapshot_is_read_without_sorting_and_a_channel_without_a_scan() {
        let store = Store::open_in_memory().unwrap();
        let conn = store.conn.lock().unwrap();
        for sql in [NEWEST_OWN, NEWEST_ANY] {
            let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let plan: Vec<String> = stmt
                .query_map(params!["work", "codex", 0, None::<i64>], |r| {
                    r.get::<_, String>(3)
                })
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            assert!(
                plan.iter().all(|step| !step.contains("TEMP B-TREE")),
                "{plan:?}"
            );
        }
        // A channel given is searched for on the index, not scanned for.
        let mut stmt = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {PAIRS_ON_CHANNEL}"))
            .unwrap();
        let plan: Vec<String> = stmt
            .query_map(params![0, None::<i64>, "work"], |r| r.get::<_, String>(3))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(
            plan.iter().all(|step| !step.starts_with("SCAN")),
            "{plan:?}"
        );
        assert!(
            plan.iter()
                .any(|step| step.starts_with("SEARCH") && step.contains("channel=?")),
            "{plan:?}"
        );
    }
}
