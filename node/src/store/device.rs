//! Device approvals (`http::device`): a window that cannot make a passkey
//! asks, a signed-in browser approves with one, and the window collects the
//! outcome by polling with a secret only it holds.
//!
//! The secret is stored as a hash, so a read of the database cannot collect
//! an approval. Every change of state is one conditional `UPDATE`: an
//! approval is decided once, and collected once, however many requests race
//! for it. Collecting is the same transaction as what it hands out (the new
//! session, or the step-up proof), so neither can be had without the other.

use rusqlite::{params, OptionalExtension};
use serde::Serialize;

use super::{AuthSessionRow, Result, Store};

/// How long a decided approval is kept, as the record of who approved what.
const RETAIN_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// The longest a window is ever asked to wait between polls.
const MAX_INTERVAL_MS: i64 = 60_000;
/// How much sooner than its interval a poll may arrive before it is told to
/// slow down: the time between two polls as the node sees them also carries
/// the network's jitter.
const POLL_LEEWAY_MS: i64 = 1_000;
/// What a window that polls too soon has its interval raised by (RFC 8628).
const SLOW_DOWN_STEP_MS: i64 = 5_000;
/// How long a window has to collect an approval decided at the last moment.
const COLLECT_GRACE_MS: i64 = 60_000;

/// What token rotation and passkey removal do to approvals still waiting or
/// not yet collected: neither may outlive the credentials that stood behind
/// it. `?1` is now.
pub(crate) const REVOKE_DEVICE_APPROVALS: &str =
    "UPDATE device_approval SET status='revoked', decided_ms=COALESCE(decided_ms, ?1)
      WHERE status IN ('pending', 'approved')";

/// One device approval.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceApprovalRow {
    pub id: String,
    /// `sign-in` or `step-up`.
    pub kind: String,
    #[serde(skip)]
    pub device_code_hash: String,
    pub user_code: String,
    /// For a step-up, the cookie session (its hash) that asked, and the only
    /// one that may collect the proof.
    #[serde(skip)]
    pub requester_session: Option<String>,
    /// What the asking window said it is: its user agent, as reported.
    pub requester_agent: Option<String>,
    /// Where the request came from, as the connection or a proxy reported.
    pub requester_addr: Option<String>,
    /// For a step-up, the operation the proof will confirm.
    pub op_hash: Option<String>,
    /// For a step-up, the request it guards and the node's line for it.
    pub action: Option<String>,
    pub summary: Option<String>,
    /// `pending`, `approved`, `denied`, `consumed` or `revoked`.
    pub status: String,
    /// What decided it: `local`, or the deciding session's hash.
    #[serde(skip)]
    pub approver: Option<String>,
    /// What collecting it issued: the new session's short ID, or `proof`.
    pub issued: Option<String>,
    pub created_ms: i64,
    pub expires_ms: i64,
    pub interval_ms: i64,
    pub last_poll_ms: Option<i64>,
    pub decided_ms: Option<i64>,
    pub consumed_ms: Option<i64>,
}

impl DeviceApprovalRow {
    fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(Self {
            id: r.get("id")?,
            kind: r.get("kind")?,
            device_code_hash: r.get("device_code_hash")?,
            user_code: r.get("user_code")?,
            requester_session: r.get("requester_session")?,
            requester_agent: r.get("requester_agent")?,
            requester_addr: r.get("requester_addr")?,
            op_hash: r.get("op_hash")?,
            action: r.get("action")?,
            summary: r.get("summary")?,
            status: r.get("status")?,
            approver: r.get("approver")?,
            issued: r.get("issued")?,
            created_ms: r.get("created_ms")?,
            expires_ms: r.get("expires_ms")?,
            interval_ms: r.get("interval_ms")?,
            last_poll_ms: r.get("last_poll_ms")?,
            decided_ms: r.get("decided_ms")?,
            consumed_ms: r.get("consumed_ms")?,
        })
    }

    /// Whether it can still be approved or denied.
    pub fn open(&self, now: i64) -> bool {
        self.status == "pending" && self.expires_ms > now
    }

    /// The state to show: `expired` for one that was never decided in time.
    pub fn shown_status(&self, now: i64) -> &str {
        if self.status == "pending" && self.expires_ms <= now {
            "expired"
        } else {
            &self.status
        }
    }
}

/// What one poll found.
#[derive(Debug)]
pub enum DevicePoll {
    /// No approval answers to that secret, or it was already collected or
    /// revoked.
    Unknown,
    /// Not decided yet.
    Pending,
    /// Polled sooner than asked: wait this long from now on.
    SlowDown {
        interval_ms: i64,
    },
    Denied,
    Expired,
    /// Approved, and ready to collect.
    Approved(Box<DeviceApprovalRow>),
}

impl Store {
    /// Record a request. Rows decided long ago are purged first. With
    /// `max_open` set, refused (false) while that many sign-in requests are
    /// already waiting: the route that makes them answers strangers, and must
    /// not let them fill the table.
    pub fn device_approval_insert(
        &self,
        row: &DeviceApprovalRow,
        max_open: Option<i64>,
        now: i64,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM device_approval WHERE created_ms <= ?1",
            [now - RETAIN_MS],
        )?;
        if let Some(max) = max_open {
            let open: i64 = conn.query_row(
                "SELECT COUNT(*) FROM device_approval
                  WHERE kind='sign-in' AND status='pending' AND expires_ms > ?1",
                [now],
                |r| r.get(0),
            )?;
            if open >= max {
                return Ok(false);
            }
        }
        conn.execute(
            "INSERT INTO device_approval
               (id, kind, device_code_hash, user_code, requester_session, requester_agent,
                requester_addr, op_hash, action, summary, status, created_ms, expires_ms,
                interval_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'pending', ?11, ?12, ?13)",
            params![
                row.id,
                row.kind,
                row.device_code_hash,
                row.user_code,
                row.requester_session,
                row.requester_agent,
                row.requester_addr,
                row.op_hash,
                row.action,
                row.summary,
                row.created_ms,
                row.expires_ms,
                row.interval_ms
            ],
        )?;
        Ok(true)
    }

    pub fn device_approval_get(&self, id: &str) -> Result<Option<DeviceApprovalRow>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT * FROM device_approval WHERE id=?1",
                [id],
                DeviceApprovalRow::from_row,
            )
            .optional()?)
    }

    pub fn device_approval_by_code(&self, code_hash: &str) -> Result<Option<DeviceApprovalRow>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT * FROM device_approval WHERE device_code_hash=?1",
                [code_hash],
                DeviceApprovalRow::from_row,
            )
            .optional()?)
    }

    /// Every approval, newest first: the record of what was approved.
    pub fn device_approvals(&self, limit: i64) -> Result<Vec<DeviceApprovalRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT * FROM device_approval ORDER BY created_ms DESC LIMIT ?1")?;
        let rows = stmt
            .query_map([limit], DeviceApprovalRow::from_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// One poll by the holder of the secret: counted against its interval,
    /// then answered with where the request stands.
    pub fn device_approval_poll(&self, code_hash: &str, now: i64) -> Result<DevicePoll> {
        let conn = self.conn.lock().unwrap();
        let Some(row) = conn
            .query_row(
                "SELECT * FROM device_approval WHERE device_code_hash=?1",
                [code_hash],
                DeviceApprovalRow::from_row,
            )
            .optional()?
        else {
            return Ok(DevicePoll::Unknown);
        };
        if matches!(row.status.as_str(), "consumed" | "revoked") {
            return Ok(DevicePoll::Unknown);
        }
        if row.status == "denied" {
            return Ok(DevicePoll::Denied);
        }
        if row.expires_ms <= now {
            return Ok(DevicePoll::Expired);
        }
        if let Some(last) = row.last_poll_ms {
            if now - last < row.interval_ms - POLL_LEEWAY_MS {
                let interval_ms = (row.interval_ms + SLOW_DOWN_STEP_MS).min(MAX_INTERVAL_MS);
                conn.execute(
                    "UPDATE device_approval SET last_poll_ms=?2, interval_ms=?3 WHERE id=?1",
                    params![row.id, now, interval_ms],
                )?;
                return Ok(DevicePoll::SlowDown { interval_ms });
            }
        }
        conn.execute(
            "UPDATE device_approval SET last_poll_ms=?2 WHERE id=?1",
            params![row.id, now],
        )?;
        Ok(if row.status == "approved" {
            DevicePoll::Approved(Box::new(row))
        } else {
            DevicePoll::Pending
        })
    }

    /// Approve or deny a request still open. `approver` is `local` or the
    /// deciding session's hash. An approval decided near its expiry is given
    /// a short grace to be collected in. Whether this call decided it.
    pub fn device_approval_decide(
        &self,
        id: &str,
        approve: bool,
        approver: &str,
        now: i64,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE device_approval
                SET status=?2, approver=?3, decided_ms=?4,
                    expires_ms=CASE WHEN ?2='approved' THEN MAX(expires_ms, ?4 + ?5) ELSE expires_ms END
              WHERE id=?1 AND status='pending' AND expires_ms > ?4",
            params![
                id,
                if approve { "approved" } else { "denied" },
                approver,
                now,
                COLLECT_GRACE_MS
            ],
        )?;
        Ok(n == 1)
    }

    /// Collect an approved sign-in: mark it collected and create the session
    /// it grants, together. False if it was not there to collect.
    pub fn device_approval_collect_sign_in(
        &self,
        id: &str,
        session: &AuthSessionRow,
        now: i64,
    ) -> Result<bool> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let n = tx.execute(
            "UPDATE device_approval SET status='consumed', consumed_ms=?2, issued=?3
              WHERE id=?1 AND kind='sign-in' AND status='approved' AND expires_ms > ?2",
            params![id, now, &session.token_hash[..12]],
        )?;
        if n != 1 {
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO auth_session
               (token_hash, created_ms, last_seen_ms, expires_ms, user_agent, paired_ms, delegated_from)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session.token_hash,
                session.created_ms,
                session.last_seen_ms,
                session.expires_ms,
                session.user_agent,
                session.paired_ms,
                session.delegated_from
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Collect an approved step-up for `session`, the one that asked: mark it
    /// collected and leave a single-use proof for its operation, bound to
    /// that session, exactly as a finished assertion leaves one
    /// (`proof_spend` spends it). False if it was not there to collect, or
    /// is not this session's.
    pub fn device_approval_collect_step_up(
        &self,
        id: &str,
        session: &str,
        proof_hash: &str,
        proof_expires_ms: i64,
        now: i64,
    ) -> Result<bool> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let Some(row) = tx
            .query_row(
                "SELECT * FROM device_approval
                  WHERE id=?1 AND kind='step-up' AND status='approved' AND expires_ms > ?2
                    AND requester_session=?3",
                params![id, now, session],
                DeviceApprovalRow::from_row,
            )
            .optional()?
        else {
            return Ok(false);
        };
        tx.execute(
            "UPDATE device_approval SET status='consumed', consumed_ms=?2, issued='proof'
              WHERE id=?1 AND status='approved'",
            params![id, now],
        )?;
        // A ceremony already spent (`used_ms`), carrying only its proof.
        tx.execute(
            "INSERT INTO webauthn_challenge
               (id, purpose, op_hash, session_hash, granted_by, label, state, created_ms,
                expires_ms, used_ms, proof_hash, proof_expires_ms, action, summary)
             VALUES (?1, 'assert', ?2, ?3, 'device', NULL, '{}', ?4, ?4, ?4, ?5, ?6, ?7, ?8)",
            params![
                format!("device-{id}"),
                row.op_hash,
                session,
                now,
                proof_hash,
                proof_expires_ms,
                row.action,
                row.summary
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, kind: &str, now: i64) -> DeviceApprovalRow {
        DeviceApprovalRow {
            id: id.into(),
            kind: kind.into(),
            device_code_hash: format!("code-{id}"),
            user_code: "BCDFGHJK".into(),
            requester_session: (kind == "step-up").then(|| "s1".into()),
            requester_agent: Some("tracon desktop".into()),
            requester_addr: None,
            op_hash: (kind == "step-up").then(|| "op-a".into()),
            action: None,
            summary: None,
            status: "pending".into(),
            approver: None,
            issued: None,
            created_ms: now,
            expires_ms: now + 600_000,
            interval_ms: 5_000,
            last_poll_ms: None,
            decided_ms: None,
            consumed_ms: None,
        }
    }

    fn session(secret: &str, now: i64) -> AuthSessionRow {
        AuthSessionRow {
            token_hash: format!("{secret:0>64}"),
            created_ms: now,
            last_seen_ms: now,
            expires_ms: now + 60_000,
            user_agent: None,
            paired_ms: None,
            delegated_from: Some("local".into()),
        }
    }

    #[test]
    fn polling_too_soon_slows_the_window_down() {
        let store = Store::open_in_memory().unwrap();
        store
            .device_approval_insert(&row("a", "sign-in", 0), None, 0)
            .unwrap();
        assert!(matches!(
            store.device_approval_poll("code-a", 0).unwrap(),
            DevicePoll::Pending
        ));
        match store.device_approval_poll("code-a", 1_000).unwrap() {
            DevicePoll::SlowDown { interval_ms } => assert_eq!(interval_ms, 10_000),
            other => panic!("{other:?}"),
        }
        // The raised interval holds from the last poll.
        assert!(matches!(
            store.device_approval_poll("code-a", 6_000).unwrap(),
            DevicePoll::SlowDown { .. }
        ));
        assert!(matches!(
            store.device_approval_poll("code-a", 26_000).unwrap(),
            DevicePoll::Pending
        ));
        assert!(matches!(
            store.device_approval_poll("nope", 30_000).unwrap(),
            DevicePoll::Unknown
        ));
    }

    #[test]
    fn an_approval_is_decided_once_and_collected_once() {
        let store = Store::open_in_memory().unwrap();
        store
            .device_approval_insert(&row("a", "sign-in", 0), None, 0)
            .unwrap();
        assert!(store
            .device_approval_decide("a", true, "local", 10)
            .unwrap());
        assert!(!store
            .device_approval_decide("a", false, "local", 11)
            .unwrap());
        assert!(matches!(
            store.device_approval_poll("code-a", 20).unwrap(),
            DevicePoll::Approved(_)
        ));
        assert!(store
            .device_approval_collect_sign_in("a", &session("x", 30), 30)
            .unwrap());
        assert!(!store
            .device_approval_collect_sign_in("a", &session("y", 31), 31)
            .unwrap());
        assert!(matches!(
            store.device_approval_poll("code-a", 10_000).unwrap(),
            DevicePoll::Unknown
        ));
        let live = store
            .auth_session_live(&format!("{:0>64}", "x"), 40)
            .unwrap()
            .unwrap();
        assert_eq!(live.delegated_from.as_deref(), Some("local"));
        assert_eq!(live.paired_ms, None);
        assert!(store
            .auth_session_live(&format!("{:0>64}", "y"), 40)
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_step_up_is_collected_only_by_the_session_that_asked() {
        let store = Store::open_in_memory().unwrap();
        store
            .device_approval_insert(&row("a", "step-up", 0), None, 0)
            .unwrap();
        // Not yet approved: nothing to collect.
        assert!(!store
            .device_approval_collect_step_up("a", "s1", "p1", 120_000, 5)
            .unwrap());
        assert!(store.device_approval_decide("a", true, "s2", 10).unwrap());
        assert!(!store
            .device_approval_collect_step_up("a", "s2", "p1", 120_000, 20)
            .unwrap());
        assert!(store
            .device_approval_collect_step_up("a", "s1", "p1", 120_000, 20)
            .unwrap());
        assert!(!store
            .device_approval_collect_step_up("a", "s1", "p2", 120_000, 21)
            .unwrap());
        // The proof is one PR 12's spend accepts, for its operation and
        // session only, once.
        assert!(!store.proof_spend("p1", "op-b", "s1", 30).unwrap());
        assert!(!store.proof_spend("p1", "op-a", "s2", 30).unwrap());
        assert!(store.proof_spend("p1", "op-a", "s1", 30).unwrap());
        assert!(!store.proof_spend("p1", "op-a", "s1", 31).unwrap());
    }

    #[test]
    fn denied_and_expired_say_so_and_cannot_be_approved_after() {
        let store = Store::open_in_memory().unwrap();
        store
            .device_approval_insert(&row("a", "sign-in", 0), None, 0)
            .unwrap();
        store
            .device_approval_insert(&row("b", "sign-in", 0), None, 0)
            .unwrap();
        assert!(store.device_approval_decide("a", false, "s9", 10).unwrap());
        assert!(matches!(
            store.device_approval_poll("code-a", 20).unwrap(),
            DevicePoll::Denied
        ));
        assert!(!store
            .device_approval_decide("b", true, "s9", 600_000)
            .unwrap());
        assert!(matches!(
            store.device_approval_poll("code-b", 600_000).unwrap(),
            DevicePoll::Expired
        ));
        let b = store.device_approval_get("b").unwrap().unwrap();
        assert_eq!(b.shown_status(600_000), "expired");
    }

    #[test]
    fn waiting_sign_ins_are_capped() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..3 {
            assert!(store
                .device_approval_insert(&row(&format!("r{i}"), "sign-in", 0), Some(3), 0)
                .unwrap());
        }
        assert!(!store
            .device_approval_insert(&row("r3", "sign-in", 0), Some(3), 0)
            .unwrap());
        // Expired ones no longer count.
        assert!(store
            .device_approval_insert(&row("r4", "sign-in", 600_000), Some(3), 600_000)
            .unwrap());
    }

    #[test]
    fn rotating_the_token_or_removing_a_passkey_revokes_what_is_uncollected() {
        let store = Store::open_in_memory().unwrap();
        store
            .device_approval_insert(&row("a", "sign-in", 0), None, 0)
            .unwrap();
        store
            .device_approval_insert(&row("b", "sign-in", 0), None, 0)
            .unwrap();
        assert!(store
            .device_approval_decide("a", true, "local", 10)
            .unwrap());
        store.set_operator_token(Some(&"0".repeat(64))).unwrap();
        assert!(matches!(
            store.device_approval_poll("code-a", 6_000).unwrap(),
            DevicePoll::Unknown
        ));
        assert!(!store
            .device_approval_collect_sign_in("a", &session("x", 20), 20)
            .unwrap());
        assert_eq!(
            store.device_approval_get("b").unwrap().unwrap().status,
            "revoked"
        );

        store
            .device_approval_insert(&row("c", "sign-in", 0), None, 0)
            .unwrap();
        assert!(store
            .device_approval_decide("c", true, "local", 10)
            .unwrap());
        store
            .passkey_insert(
                &crate::store::PasskeyRow {
                    id: "k".into(),
                    passkey: "{}".into(),
                    sign_count: 0,
                    backup_eligible: false,
                    backup_state: false,
                    transports: None,
                    label: "k".into(),
                    created_ms: 0,
                    last_used_ms: None,
                },
                false,
            )
            .unwrap();
        assert!(store.passkey_delete("k").unwrap());
        assert_eq!(
            store.device_approval_get("c").unwrap().unwrap().status,
            "revoked"
        );
    }
}
