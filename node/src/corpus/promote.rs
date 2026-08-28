//! Promotion on the node: the nightly batch for channels this node
//! processes, and the operator's verdicts on any batch. A channel whose
//! binding says `processing: "hub"` is the hub's to batch; verdicts are
//! always local writes and converge like any other change.

use std::sync::Arc;

use serde_json::Value;
use tracon_sync::{batch, ChangeOp};

use crate::{
    config::Config,
    corpus,
    store::{now_ms, Store},
    stream::{Bus, Frame},
};

/// A candidate must be at least this old before it is batched, so a session
/// still running is not asked about mid-thought.
pub const MIN_AGE_MS: i64 = 6 * 3600 * 1000;

/// Whether this node batches a channel: not when the hub does.
pub fn batches_here(store: &Store, channel: &str) -> bool {
    store
        .channel_get(channel)
        .ok()
        .flatten()
        .and_then(|c| serde_json::from_str::<Value>(&c.bindings_json).ok())
        .map(|b| b["processing"].as_str() != Some("hub"))
        .unwrap_or(true)
}

/// Build batches now for every channel this node processes. Returns the
/// promotion ids created.
pub fn batch_now(store: &Store, bus: &Bus, site: &str, min_age_ms: i64) -> Vec<String> {
    let mut channels: Vec<String> = store
        .channel_list()
        .unwrap_or_default()
        .into_iter()
        .map(|c| c.name)
        .filter(|c| !c.starts_with('@'))
        .collect();
    if channels.is_empty() {
        // A standalone node keeps channels as labels: batch whatever exists.
        channels = store.memory_channels().unwrap_or_default();
    }
    let mut out = Vec::new();
    for channel in channels {
        if !batches_here(store, &channel) {
            continue;
        }
        let id = corpus::new_id();
        let plan = {
            let conn = store.conn();
            batch::plan_promotion(&conn, &channel, &id, now_ms(), min_age_ms)
        };
        let Ok(Some(plan)) = plan else {
            continue;
        };
        if corpus::write(
            store,
            bus,
            site,
            &channel,
            "promotion",
            ChangeOp::Upsert,
            &id,
            plan.promotion_row(now_ms()),
        )
        .is_err()
        {
            continue;
        }
        for item in &plan.items {
            let row = {
                let conn = store.conn();
                batch::memory_row_with_state(&conn, &item.memory_id, "proposed", now_ms())
            };
            if let Ok(Some(row)) = row {
                let _ = corpus::write(
                    store,
                    bus,
                    site,
                    &channel,
                    "memory",
                    ChangeOp::Upsert,
                    &item.memory_id,
                    row,
                );
            }
        }
        out.push(id);
    }
    if !out.is_empty() {
        publish(store, bus);
    }
    out
}

/// The operator decided some or all of a batch.
pub fn decide(
    store: &Store,
    bus: &Bus,
    site: &str,
    promotion_id: &str,
    verdicts: &serde_json::Map<String, Value>,
) -> Result<bool, String> {
    let planned = {
        let conn = store.conn();
        batch::plan_verdict(&conn, promotion_id, verdicts, site, now_ms())
            .map_err(|e| e.to_string())?
    };
    let Some((row, memories)) = planned else {
        return Ok(false);
    };
    let channel = row["channel"].as_str().unwrap_or("").to_string();
    for (id, state) in memories {
        let mrow = {
            let conn = store.conn();
            batch::memory_row_with_state(&conn, &id, state, now_ms()).map_err(|e| e.to_string())?
        };
        if let Some(mrow) = mrow {
            corpus::write(
                store,
                bus,
                site,
                &channel,
                "memory",
                ChangeOp::Upsert,
                &id,
                mrow,
            )
            .map_err(|e| e.to_string())?;
        }
    }
    corpus::write(
        store,
        bus,
        site,
        &channel,
        "promotion",
        ChangeOp::Upsert,
        promotion_id,
        row,
    )
    .map_err(|e| e.to_string())?;
    publish(store, bus);
    Ok(true)
}

/// Push the open batches to the interface.
pub fn publish(store: &Store, bus: &Bus) {
    let waiting = store.open_promotions().unwrap_or_default();
    bus.publish_untapped(Frame::Promotions { waiting });
}

/// Seconds until the next `HH:MM` local time.
pub fn secs_until(at: &str, now: chrono_free::LocalTime) -> u64 {
    let (h, m) = at
        .split_once(':')
        .and_then(|(h, m)| Some((h.parse::<u32>().ok()?, m.parse::<u32>().ok()?)))
        .unwrap_or((2, 0));
    let target = (h * 3600 + m * 60) as i64;
    let now_s = (now.hour * 3600 + now.minute * 60 + now.second) as i64;
    let mut diff = target - now_s;
    if diff <= 0 {
        diff += 86_400;
    }
    diff as u64
}

/// Enough of a local clock to schedule a nightly job without a date crate.
pub mod chrono_free {
    pub struct LocalTime {
        pub hour: u32,
        pub minute: u32,
        pub second: u32,
    }

    pub fn offset_minutes() -> i64 {
        std::env::var("TRACON_TZ_OFFSET_MINUTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }

    /// The wall-clock millisecond the local day began: the ceiling's "today".
    pub fn day_start_ms() -> i64 {
        let offset_ms = offset_minutes() * 60_000;
        let local = crate::store::now_ms() + offset_ms;
        local - local.rem_euclid(86_400_000) - offset_ms
    }

    /// UTC, offset by `TZ_OFFSET_MINUTES` if set; a node in a pod has no
    /// zoneinfo worth trusting and a laptop's operator can set it.
    pub fn now() -> LocalTime {
        let offset: i64 = offset_minutes();
        let secs = crate::store::now_ms() / 1000 + offset * 60;
        let day = secs.rem_euclid(86_400) as u32;
        LocalTime {
            hour: day / 3600,
            minute: (day % 3600) / 60,
            second: day % 60,
        }
    }
}

/// The nightly loop.
pub async fn nightly(store: Arc<Store>, bus: Bus, site: String, cfg: Arc<Config>) {
    loop {
        let wait = secs_until(&cfg.memory.promote_at, chrono_free::now());
        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
        let made = batch_now(&store, &bus, &site, MIN_AGE_MS);
        if !made.is_empty() {
            tracing::info!(batches = made.len(), "nightly promotion batches built");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seconds_until_wraps_past_midnight() {
        let t = chrono_free::LocalTime {
            hour: 1,
            minute: 0,
            second: 0,
        };
        assert_eq!(secs_until("02:00", t), 3600);
        let t = chrono_free::LocalTime {
            hour: 3,
            minute: 0,
            second: 30,
        };
        assert_eq!(secs_until("02:00", t), 23 * 3600 - 30);
        let t = chrono_free::LocalTime {
            hour: 2,
            minute: 0,
            second: 0,
        };
        assert_eq!(secs_until("02:00", t), 86_400);
    }
}
