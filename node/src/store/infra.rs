//! What infrastructure cost: each sandbox an elastic provider ran for this
//! node, from create to destroy, priced by the hour at the rate its size had
//! when it was made. Beside the gateway's token usage (inference), this is
//! the other half of what a session cost.
//!
//! A sandbox is billed while it exists, not while its agent is busy (it is
//! kept warm), so its cost is wall-clock hours times its hourly price; a row
//! still running is priced up to now.

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::{Result, Store};

const HOUR_MS: f64 = 3_600_000.0;

/// One sandbox's lifetime.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SandboxUsage {
    /// The provider's handle for it.
    pub sandbox_id: String,
    /// `digitalocean`.
    pub provider: String,
    /// The name the node knows it by (a session's container name).
    pub name: Option<String>,
    /// `RunKind::as_str`: `session`, `check`, `prepare`, `probe`,
    /// `fetch`.
    pub kind: String,
    pub session_id: Option<String>,
    pub channel: Option<String>,
    /// The size slug it was made at.
    pub size: Option<String>,
    /// Null when the size's price could not be read: the row is unpriced.
    pub price_per_hour_usd: Option<f64>,
    pub started_ms: i64,
    pub ended_ms: Option<i64>,
    pub end_reason: Option<String>,
}

impl SandboxUsage {
    /// The milliseconds of its life inside `[since_ms, until_ms)`, a running
    /// one's counted up to `now_ms`.
    pub fn ms_between(&self, since_ms: i64, until_ms: Option<i64>, now_ms: i64) -> i64 {
        let end = self
            .ended_ms
            .unwrap_or(now_ms)
            .min(until_ms.unwrap_or(i64::MAX));
        (end - self.started_ms.max(since_ms)).max(0)
    }

    pub fn hours_between(&self, since_ms: i64, until_ms: Option<i64>, now_ms: i64) -> f64 {
        self.ms_between(since_ms, until_ms, now_ms) as f64 / HOUR_MS
    }

    /// What its life inside the window cost, or `None` when it is unpriced.
    pub fn cost_between(&self, since_ms: i64, until_ms: Option<i64>, now_ms: i64) -> Option<f64> {
        self.price_per_hour_usd
            .map(|p| p * self.hours_between(since_ms, until_ms, now_ms))
    }

    /// What its whole life has cost so far.
    pub fn cost(&self, now_ms: i64) -> Option<f64> {
        self.cost_between(i64::MIN, None, now_ms)
    }
}

fn usage_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SandboxUsage> {
    Ok(SandboxUsage {
        sandbox_id: r.get("sandbox_id")?,
        provider: r.get("provider")?,
        name: r.get("name")?,
        kind: r.get("kind")?,
        session_id: r.get("session_id")?,
        channel: r.get("channel")?,
        size: r.get("size")?,
        price_per_hour_usd: r.get("price_per_hour_usd")?,
        started_ms: r.get("started_ms")?,
        ended_ms: r.get("ended_ms")?,
        end_reason: r.get("end_reason")?,
    })
}

impl Store {
    /// Note a sandbox made. The session is looked up by `name` (its
    /// container name, recorded before the sandbox exists) when the caller
    /// does not know it. A second note of the same sandbox is ignored.
    pub fn sandbox_started(&self, u: &SandboxUsage) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let (session_id, channel) = match (&u.session_id, &u.name) {
            (Some(_), _) | (None, None) => (u.session_id.clone(), u.channel.clone()),
            (None, Some(name)) => conn
                .query_row(
                    "SELECT id, channel FROM session WHERE container_name = ?1
                     ORDER BY created_ms DESC LIMIT 1",
                    params![name],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                )
                .optional()?
                .map_or((None, u.channel.clone()), |(id, ch)| {
                    (Some(id), u.channel.clone().or(Some(ch)))
                }),
        };
        conn.execute(
            "INSERT OR IGNORE INTO sandbox_usage (sandbox_id, provider, name, kind, session_id,
                channel, size, price_per_hour_usd, started_ms, ended_ms, end_reason)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                u.sandbox_id,
                u.provider,
                u.name,
                u.kind,
                session_id,
                channel,
                u.size,
                u.price_per_hour_usd,
                u.started_ms,
                u.ended_ms,
                u.end_reason,
            ],
        )?;
        Ok(())
    }

    /// Note a sandbox gone. Only its first end counts; `false` when it was
    /// not running (never noted, or ended already).
    pub fn sandbox_ended(&self, sandbox_id: &str, ended_ms: i64, reason: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE sandbox_usage SET ended_ms = max(?2, started_ms), end_reason = ?3
             WHERE sandbox_id = ?1 AND ended_ms IS NULL",
            params![sandbox_id, ended_ms, reason],
        )?;
        Ok(n > 0)
    }

    /// The sandboxes on `provider` not yet seen to end.
    pub fn running_sandboxes(&self, provider: &str) -> Result<Vec<SandboxUsage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM sandbox_usage WHERE provider = ?1 AND ended_ms IS NULL
             ORDER BY started_ms",
        )?;
        let rows = stmt
            .query_map(params![provider], usage_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Every sandbox alive at some point in `[since_ms, until_ms)`, on
    /// `channel` when one is given, oldest first.
    pub fn sandboxes_between(
        &self,
        since_ms: i64,
        until_ms: Option<i64>,
        channel: Option<&str>,
    ) -> Result<Vec<SandboxUsage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM sandbox_usage
             WHERE (ended_ms IS NULL OR ended_ms > ?1)
               AND (?2 IS NULL OR started_ms < ?2)
               AND (?3 IS NULL OR channel = ?3)
             ORDER BY started_ms, sandbox_id",
        )?;
        let rows = stmt
            .query_map(params![since_ms, until_ms, channel], usage_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The sandboxes one session ran in, oldest first.
    pub fn session_sandboxes(&self, session_id: &str) -> Result<Vec<SandboxUsage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM sandbox_usage WHERE session_id = ?1 ORDER BY started_ms, sandbox_id",
        )?;
        let rows = stmt
            .query_map(params![session_id], usage_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, started: i64, ended: Option<i64>, price: Option<f64>) -> SandboxUsage {
        SandboxUsage {
            sandbox_id: id.into(),
            provider: "digitalocean".into(),
            kind: "session".into(),
            price_per_hour_usd: price,
            started_ms: started,
            ended_ms: ended,
            ..Default::default()
        }
    }

    #[test]
    fn a_sandbox_costs_its_hours_inside_the_window_at_its_price() {
        let h = HOUR_MS as i64;
        let u = row("a", 0, Some(3 * h), Some(0.5));
        assert_eq!(u.cost(10 * h), Some(1.5));
        // Only the last two hours fall in the window.
        assert_eq!(u.cost_between(h, None, 10 * h), Some(1.0));
        // A window that closes before it ended.
        assert_eq!(u.cost_between(0, Some(h), 10 * h), Some(0.5));
        // A running one is priced up to now.
        let running = row("b", 0, None, Some(1.0));
        assert_eq!(running.cost(2 * h), Some(2.0));
        // Unpriced stays unpriced, but its hours still count.
        let unpriced = row("c", 0, Some(h), None);
        assert_eq!(unpriced.cost(h), None);
        assert_eq!(unpriced.hours_between(0, None, h), 1.0);
    }

    #[test]
    fn a_sandbox_is_tied_to_its_session_by_container_name_and_ends_once() {
        let store = Store::open_in_memory().unwrap();
        let node = crate::store::tests::node(&store);
        crate::store::tests::session(&store, "s1", &node);
        store
            .update_session(
                "s1",
                crate::store::SessionPatch {
                    container_name: Some("tracon-h-s1".into()),
                    ..Default::default()
                },
            )
            .unwrap();

        let mut u = row("do-1", 1_000, None, Some(0.25));
        u.name = Some("tracon-h-s1".into());
        store.sandbox_started(&u).unwrap();
        // A second note of the same sandbox changes nothing.
        store.sandbox_started(&u).unwrap();

        let rows = store.session_sandboxes("s1").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].channel.as_deref(), Some("personal"));
        assert_eq!(store.running_sandboxes("digitalocean").unwrap().len(), 1);

        assert!(store.sandbox_ended("do-1", 5_000, "destroyed").unwrap());
        assert!(!store.sandbox_ended("do-1", 9_000, "lost").unwrap());
        let rows = store.session_sandboxes("s1").unwrap();
        assert_eq!(rows[0].ended_ms, Some(5_000));
        assert_eq!(rows[0].end_reason.as_deref(), Some("destroyed"));
        assert!(store.running_sandboxes("digitalocean").unwrap().is_empty());

        // In the window it overlapped, on its channel; not before it.
        assert_eq!(
            store
                .sandboxes_between(0, None, Some("personal"))
                .unwrap()
                .len(),
            1
        );
        assert!(store
            .sandboxes_between(6_000, None, None)
            .unwrap()
            .is_empty());
        assert!(store
            .sandboxes_between(0, Some(1_000), None)
            .unwrap()
            .is_empty());
        assert!(store
            .sandboxes_between(0, None, Some("other"))
            .unwrap()
            .is_empty());
    }
}
