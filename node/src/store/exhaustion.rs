//! Each session's exhaustion policy and its latest exhaustion.
//!
//! The policy is written when the session is made. When its provider is
//! exhausted the decision is written here before the session is fenced, the
//! safe boundary once the fenced turn has settled, and the outcome when the
//! node wakes it, carries it on, or hands it to the operator. The session's
//! log carries the same facts as events; this row is what the wake sweep
//! reads, so a restart in between loses nothing.

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::{now_ms, Result, Store};
use crate::session::exhaustion::{Choice, Decision, Policy};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExhaustionRow {
    pub session_id: String,
    pub policy: String,
    pub fallback: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub status: Option<i64>,
    pub reason: Option<String>,
    pub reset_ms: Option<i64>,
    pub next_wake_ms: Option<i64>,
    pub decided_ms: Option<i64>,
    /// The last event seq of the session when its fenced turn had settled:
    /// nothing resumes or continues before this is set.
    pub boundary_seq: Option<i64>,
    /// `waiting`, `falling_back`, `held`, then `resumed`, `continued`,
    /// `operator` or `abandoned`. `None` until the provider is exhausted.
    pub outcome: Option<String>,
    pub note: Option<String>,
    pub continued_by: Option<String>,
    pub updated_ms: i64,
}

impl ExhaustionRow {
    fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            session_id: r.get("session_id")?,
            policy: r.get("policy")?,
            fallback: r.get("fallback")?,
            provider: r.get("provider")?,
            model: r.get("model")?,
            status: r.get("status")?,
            reason: r.get("reason")?,
            reset_ms: r.get("reset_ms")?,
            next_wake_ms: r.get("next_wake_ms")?,
            decided_ms: r.get("decided_ms")?,
            boundary_seq: r.get("boundary_seq")?,
            outcome: r.get("outcome")?,
            note: r.get("note")?,
            continued_by: r.get("continued_by")?,
            updated_ms: r.get("updated_ms")?,
        })
    }

    pub fn choice(&self) -> Choice {
        Choice {
            policy: Policy::parse(&self.policy).unwrap_or_default(),
            fallback: self.fallback.clone(),
        }
    }

    /// Exhausted and not yet settled one way or another.
    pub fn pending(&self) -> bool {
        matches!(
            self.outcome.as_deref(),
            Some(crate::session::exhaustion::WAITING)
                | Some(crate::session::exhaustion::FALLING_BACK)
                | Some(crate::session::exhaustion::HELD)
        )
    }
}

/// One exhaustion as the gateway saw it.
#[derive(Debug, Clone)]
pub struct Exhausted {
    pub provider: String,
    pub model: String,
    pub status: u16,
    pub reason: String,
    pub reset_ms: Option<i64>,
}

impl Store {
    /// The session's policy, chosen when it is made.
    pub fn exhaustion_choose(&self, session_id: &str, choice: &Choice) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO exhaustion (session_id, policy, fallback, updated_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(session_id) DO UPDATE SET policy=?2, fallback=?3, updated_ms=?4",
            params![
                session_id,
                choice.policy.as_str(),
                choice.fallback,
                now_ms()
            ],
        )?;
        Ok(())
    }

    pub fn exhaustion(&self, session_id: &str) -> Result<Option<ExhaustionRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT * FROM exhaustion WHERE session_id=?1",
            [session_id],
            ExhaustionRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Record what was decided. The boundary and any earlier outcome are
    /// cleared: this is a new exhaustion, and its boundary is still ahead.
    pub fn exhaustion_decided(
        &self,
        session_id: &str,
        seen: &Exhausted,
        decision: &Decision,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = now_ms();
        conn.execute(
            "INSERT INTO exhaustion (session_id, policy, fallback, provider, model, status,
                 reason, reset_ms, next_wake_ms, decided_ms, boundary_seq, outcome, note,
                 continued_by, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL, ?11, ?12, NULL, ?10)
             ON CONFLICT(session_id) DO UPDATE SET
                 provider=?4, model=?5, status=?6, reason=?7, reset_ms=?8, next_wake_ms=?9,
                 decided_ms=?10, boundary_seq=NULL, outcome=?11, note=?12,
                 continued_by=NULL, updated_ms=?10",
            params![
                session_id,
                decision.policy.as_str(),
                decision.fallback,
                seen.provider,
                seen.model,
                seen.status,
                seen.reason,
                seen.reset_ms,
                decision.next_wake_ms,
                now,
                decision.outcome,
                decision.note,
            ],
        )?;
        Ok(())
    }

    /// The fenced turn has settled: everything up to the session's latest
    /// event is what the work stands on. Returns the seq written.
    pub fn exhaustion_boundary(&self, session_id: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let seq: i64 = conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM event WHERE session_id=?1",
            [session_id],
            |r| r.get(0),
        )?;
        conn.execute(
            "UPDATE exhaustion SET boundary_seq=?2, updated_ms=?3
              WHERE session_id=?1 AND boundary_seq IS NULL",
            params![session_id, seq, now_ms()],
        )?;
        Ok(seq)
    }

    /// Settle a pending exhaustion. Guarded on it still being pending, so a
    /// wake and an operator's resume cannot both claim it; `false` when it
    /// was not.
    pub fn exhaustion_settle(
        &self,
        session_id: &str,
        outcome: &str,
        note: Option<&str>,
        continued_by: Option<&str>,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE exhaustion
                SET outcome=?2, note=COALESCE(?3, note),
                    continued_by=COALESCE(?4, continued_by), updated_ms=?5
              WHERE session_id=?1 AND outcome IN ('waiting','falling_back','held')",
            params![session_id, outcome, note, continued_by, now_ms()],
        )?;
        Ok(n > 0)
    }

    /// Hold a pending exhaustion for the operator: the wake found something
    /// that would refuse the session.
    pub fn exhaustion_hold(&self, session_id: &str, note: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE exhaustion SET outcome='held', note=?2, next_wake_ms=NULL, updated_ms=?3
              WHERE session_id=?1 AND outcome IN ('waiting','falling_back')",
            params![session_id, note, now_ms()],
        )?;
        Ok(n > 0)
    }

    /// Exhaustions the node acts on now: a wait whose reset has passed, or a
    /// fallback, either one only once its boundary is recorded.
    pub fn exhaustion_due(&self, now: i64) -> Result<Vec<ExhaustionRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM exhaustion
              WHERE boundary_seq IS NOT NULL
                AND ((outcome='waiting' AND next_wake_ms <= ?1) OR outcome='falling_back')
              ORDER BY decided_ms",
        )?;
        let rows = stmt
            .query_map([now], ExhaustionRow::from_row)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }
}
