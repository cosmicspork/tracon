//! Passkeys and the ceremonies that use them (`http::passkey`).
//!
//! A credential row holds only public material: the verifier's record of the
//! credential, its counter and flags. A ceremony row holds the verifier's
//! server-side state for one registration or assertion. Every row is bound
//! to the cookie session that asked for it; the proof a finished assertion
//! leaves behind is stored as a hash, so a read of the database cannot spend
//! one. Each of challenge and proof is spent by a single conditional
//! `UPDATE`, so two requests racing for the same one cannot both win.

use rusqlite::{params, OptionalExtension};
use serde::Serialize;

use super::{Result, Store};

/// One enrolled passkey.
#[derive(Debug, Clone, Serialize)]
pub struct PasskeyRow {
    /// The credential ID, base64url.
    pub id: String,
    /// The verifier's record of the credential, as JSON. Public: a public
    /// key, a counter and flags.
    #[serde(skip)]
    pub passkey: String,
    pub sign_count: i64,
    pub backup_eligible: bool,
    pub backup_state: bool,
    /// The transports the browser reported at enrollment, as a JSON array.
    pub transports: Option<String>,
    pub label: String,
    pub created_ms: i64,
    pub last_used_ms: Option<i64>,
}

impl PasskeyRow {
    fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(Self {
            id: r.get("id")?,
            passkey: r.get("passkey")?,
            sign_count: r.get("sign_count")?,
            backup_eligible: r.get::<_, i64>("backup_eligible")? != 0,
            backup_state: r.get::<_, i64>("backup_state")? != 0,
            transports: r.get("transports")?,
            label: r.get("label")?,
            created_ms: r.get("created_ms")?,
            last_used_ms: r.get("last_used_ms")?,
        })
    }
}

/// What a ceremony is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CeremonyPurpose {
    Register,
    Assert,
}

impl CeremonyPurpose {
    fn as_str(self) -> &'static str {
        match self {
            CeremonyPurpose::Register => "register",
            CeremonyPurpose::Assert => "assert",
        }
    }
}

/// One ceremony in flight.
#[derive(Debug, Clone)]
pub struct CeremonyRow {
    pub id: String,
    pub purpose: CeremonyPurpose,
    /// The operation an assertion confirms; `None` for a registration.
    pub op_hash: Option<String>,
    /// The cookie session (its hash) the ceremony belongs to, or `local`.
    pub session_hash: String,
    /// For a registration, what allowed it: `local`, `window` or `step_up`.
    pub granted_by: Option<String>,
    /// For a registration, the name the operator gave the passkey.
    pub label: Option<String>,
    /// The verifier's state for this ceremony, as JSON.
    pub state: String,
    pub created_ms: i64,
    pub expires_ms: i64,
    /// For an assertion, the request it confirms (`METHOD /path`) and the
    /// node's line for it, kept so a device approval made from the
    /// challenge shows the same words (`http::device`).
    pub action: Option<String>,
    pub summary: Option<String>,
}

impl CeremonyRow {
    fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Self> {
        let purpose: String = r.get("purpose")?;
        Ok(Self {
            id: r.get("id")?,
            purpose: if purpose == "register" {
                CeremonyPurpose::Register
            } else {
                CeremonyPurpose::Assert
            },
            op_hash: r.get("op_hash")?,
            session_hash: r.get("session_hash")?,
            granted_by: r.get("granted_by")?,
            label: r.get("label")?,
            state: r.get("state")?,
            created_ms: r.get("created_ms")?,
            expires_ms: r.get("expires_ms")?,
            action: r.get("action")?,
            summary: r.get("summary")?,
        })
    }
}

/// The most ceremonies one session may have open at once. A client that asks
/// for more is abandoning the earlier ones; the oldest go first.
const OPEN_PER_SESSION: i64 = 8;

impl Store {
    pub fn passkeys(&self) -> Result<Vec<PasskeyRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM webauthn_credential ORDER BY created_ms")?;
        let rows = stmt
            .query_map([], PasskeyRow::from_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    pub fn passkey_count(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row("SELECT COUNT(*) FROM webauthn_credential", [], |r| r.get(0))?)
    }

    /// Enroll a passkey. With `only_if_first`, only while none is enrolled,
    /// decided in the same statement as the insert: the pairing window
    /// admits the node's first passkey and no other, however many paired
    /// clients race for it. Whether the row went in.
    pub fn passkey_insert(&self, row: &PasskeyRow, only_if_first: bool) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let guard = if only_if_first {
            " WHERE NOT EXISTS (SELECT 1 FROM webauthn_credential)"
        } else {
            ""
        };
        let n = conn.execute(
            &format!(
                "INSERT OR IGNORE INTO webauthn_credential
                   (id, passkey, sign_count, backup_eligible, backup_state, transports, label,
                    created_ms, last_used_ms)
                 SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9{guard}"
            ),
            params![
                row.id,
                row.passkey,
                row.sign_count,
                row.backup_eligible as i64,
                row.backup_state as i64,
                row.transports,
                row.label,
                row.created_ms,
                row.last_used_ms
            ],
        )?;
        Ok(n == 1)
    }

    /// Record a use: the verifier's updated record, its counter and flags.
    pub fn passkey_used(
        &self,
        id: &str,
        passkey: &str,
        sign_count: i64,
        backup_eligible: bool,
        backup_state: bool,
        now: i64,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE webauthn_credential
                SET passkey=?2, sign_count=?3, backup_eligible=?4, backup_state=?5, last_used_ms=?6
              WHERE id=?1",
            params![
                id,
                passkey,
                sign_count,
                backup_eligible as i64,
                backup_state as i64,
                now
            ],
        )?;
        Ok(())
    }

    /// Remove a passkey, and with it every ceremony in flight, every unspent
    /// proof and every device approval not yet collected: none records which
    /// passkey made it, so none made before the removal outlives it. Whether
    /// there was one by that ID.
    pub fn passkey_delete(&self, id: &str) -> Result<bool> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let removed = tx.execute("DELETE FROM webauthn_credential WHERE id=?1", [id])? == 1;
        if removed {
            tx.execute("DELETE FROM webauthn_challenge", [])?;
            tx.execute(super::device::REVOKE_DEVICE_APPROVALS, [super::now_ms()])?;
        }
        tx.commit()?;
        Ok(removed)
    }

    /// Open a ceremony. Expired rows, and the oldest of this session's beyond
    /// [`OPEN_PER_SESSION`], are cleared first, so the table stays small
    /// however often a client asks.
    pub fn ceremony_insert(&self, row: &CeremonyRow, now: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM webauthn_challenge
              WHERE expires_ms <= ?1 AND (proof_expires_ms IS NULL OR proof_expires_ms <= ?1)",
            [now],
        )?;
        conn.execute(
            "DELETE FROM webauthn_challenge WHERE id IN (
                SELECT id FROM webauthn_challenge WHERE session_hash=?1
                 ORDER BY created_ms DESC, id DESC LIMIT -1 OFFSET ?2)",
            params![row.session_hash, OPEN_PER_SESSION - 1],
        )?;
        conn.execute(
            "INSERT INTO webauthn_challenge
               (id, purpose, op_hash, session_hash, granted_by, label, state, created_ms, expires_ms,
                action, summary)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                row.id,
                row.purpose.as_str(),
                row.op_hash,
                row.session_hash,
                row.granted_by,
                row.label,
                row.state,
                row.created_ms,
                row.expires_ms,
                row.action,
                row.summary
            ],
        )?;
        Ok(())
    }

    /// What the newest assertion `session` opened for `op_hash` said it
    /// confirms, and its `granted_by`, so a fresh challenge for the same
    /// operation says the same and is held to the same.
    pub fn ceremony_describe(
        &self,
        session_hash: &str,
        op_hash: &str,
    ) -> Result<(Option<String>, Option<String>, Option<String>)> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT action, summary, granted_by FROM webauthn_challenge
                  WHERE session_hash=?1 AND op_hash=?2 AND purpose='assert'
                  ORDER BY created_ms DESC LIMIT 1",
                params![session_hash, op_hash],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .unwrap_or((None, None, None)))
    }

    /// Spend a ceremony: the one with this ID, purpose and session, unused
    /// and unexpired, now marked used. It is spent whether or not what the
    /// client answers with then verifies, so a challenge is never tried
    /// twice.
    pub fn ceremony_take(
        &self,
        id: &str,
        purpose: CeremonyPurpose,
        session_hash: &str,
        now: i64,
    ) -> Result<Option<CeremonyRow>> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let n = tx.execute(
            "UPDATE webauthn_challenge SET used_ms=?4
              WHERE id=?1 AND purpose=?2 AND session_hash=?3 AND used_ms IS NULL AND expires_ms > ?4",
            params![id, purpose.as_str(), session_hash, now],
        )?;
        if n != 1 {
            return Ok(None);
        }
        let row = tx
            .query_row(
                "SELECT * FROM webauthn_challenge WHERE id=?1",
                [id],
                CeremonyRow::from_row,
            )
            .optional()?;
        tx.commit()?;
        Ok(row)
    }

    /// Leave a step-up proof on a finished assertion: the hash of the secret
    /// the client was handed, and until when it may be spent.
    pub fn ceremony_prove(&self, id: &str, proof_hash: &str, expires_ms: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE webauthn_challenge SET proof_hash=?2, proof_expires_ms=?3
              WHERE id=?1 AND purpose='assert' AND proof_hash IS NULL",
            params![id, proof_hash, expires_ms],
        )?;
        Ok(())
    }

    /// Spend a step-up proof on the operation it was given for. True only for
    /// the first spend of an unexpired proof whose operation and session both
    /// match; a proof presented for another operation is refused and stays
    /// unspent, still good for its own.
    pub fn proof_spend(
        &self,
        proof_hash: &str,
        op_hash: &str,
        session_hash: &str,
        now: i64,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE webauthn_challenge SET proof_used_ms=?4
              WHERE proof_hash=?1 AND op_hash=?2 AND session_hash=?3 AND purpose='assert'
                AND proof_used_ms IS NULL AND proof_expires_ms > ?4",
            params![proof_hash, op_hash, session_hash, now],
        )?;
        Ok(n == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passkey(id: &str) -> PasskeyRow {
        PasskeyRow {
            id: id.into(),
            passkey: "{}".into(),
            sign_count: 0,
            backup_eligible: true,
            backup_state: true,
            transports: Some(r#"["internal","hybrid"]"#.into()),
            label: "phone".into(),
            created_ms: 1,
            last_used_ms: None,
        }
    }

    fn ceremony(id: &str, purpose: CeremonyPurpose, session: &str, created: i64) -> CeremonyRow {
        CeremonyRow {
            id: id.into(),
            purpose,
            op_hash: (purpose == CeremonyPurpose::Assert).then(|| "op-a".into()),
            session_hash: session.into(),
            granted_by: None,
            label: None,
            state: "{}".into(),
            created_ms: created,
            expires_ms: created + 120_000,
            action: None,
            summary: None,
        }
    }

    #[test]
    fn only_the_first_passkey_goes_in_under_the_first_only_rule() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.passkey_insert(&passkey("a"), true).unwrap());
        assert!(!store.passkey_insert(&passkey("b"), true).unwrap());
        assert!(store.passkey_insert(&passkey("b"), false).unwrap());
        assert_eq!(store.passkey_count().unwrap(), 2);
        // The same credential twice is one credential.
        assert!(!store.passkey_insert(&passkey("b"), false).unwrap());
        store
            .ceremony_insert(&ceremony("c1", CeremonyPurpose::Assert, "s1", 1_000), 1_000)
            .unwrap();
        store.ceremony_prove("c1", "p1", 9_000).unwrap();
        assert!(store.passkey_delete("a").unwrap());
        assert!(!store.passkey_delete("a").unwrap());
        // Removing a passkey leaves no proof made before it standing.
        assert!(!store.proof_spend("p1", "op-a", "s1", 1_500).unwrap());
        let left = store.passkeys().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(
            left[0].transports.as_deref(),
            Some(r#"["internal","hybrid"]"#)
        );
    }

    #[test]
    fn a_ceremony_is_spent_once_by_its_own_session_before_it_expires() {
        let store = Store::open_in_memory().unwrap();
        let row = ceremony("c1", CeremonyPurpose::Register, "s1", 1_000);
        store.ceremony_insert(&row, 1_000).unwrap();
        // Another session, the other purpose, or too late: not spent.
        assert!(store
            .ceremony_take("c1", CeremonyPurpose::Register, "s2", 1_001)
            .unwrap()
            .is_none());
        assert!(store
            .ceremony_take("c1", CeremonyPurpose::Assert, "s1", 1_001)
            .unwrap()
            .is_none());
        assert!(store
            .ceremony_take("c1", CeremonyPurpose::Register, "s1", 121_000)
            .unwrap()
            .is_none());
        let taken = store
            .ceremony_take("c1", CeremonyPurpose::Register, "s1", 1_001)
            .unwrap()
            .unwrap();
        assert_eq!(taken.purpose, CeremonyPurpose::Register);
        assert!(store
            .ceremony_take("c1", CeremonyPurpose::Register, "s1", 1_002)
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_proof_is_spent_once_on_its_own_operation_and_session() {
        let store = Store::open_in_memory().unwrap();
        store
            .ceremony_insert(&ceremony("c1", CeremonyPurpose::Assert, "s1", 1_000), 1_000)
            .unwrap();
        store.ceremony_prove("c1", "p1", 2_000).unwrap();
        assert!(!store.proof_spend("p1", "op-b", "s1", 1_500).unwrap());
        assert!(!store.proof_spend("p1", "op-a", "s2", 1_500).unwrap());
        assert!(!store.proof_spend("p1", "op-a", "s1", 2_000).unwrap());
        assert!(store.proof_spend("p1", "op-a", "s1", 1_500).unwrap());
        assert!(!store.proof_spend("p1", "op-a", "s1", 1_501).unwrap());
    }

    #[test]
    fn open_ceremonies_are_bounded_per_session_and_expire() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..20 {
            store
                .ceremony_insert(
                    &ceremony(&format!("c{i}"), CeremonyPurpose::Assert, "s1", 1_000 + i),
                    1_000 + i,
                )
                .unwrap();
        }
        let conn = store.conn.lock().unwrap();
        let open: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM webauthn_challenge WHERE session_hash='s1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(open, OPEN_PER_SESSION);
        drop(conn);
        // The newest survived.
        assert!(store
            .ceremony_take("c19", CeremonyPurpose::Assert, "s1", 1_100)
            .unwrap()
            .is_some());
        // A ceremony long expired is gone by the next insert.
        store
            .ceremony_insert(
                &ceremony("late", CeremonyPurpose::Assert, "s9", 999_999),
                999_999,
            )
            .unwrap();
        let conn = store.conn.lock().unwrap();
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM webauthn_challenge", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 1);
    }

    #[test]
    fn one_session_flooding_ceremonies_evicts_only_its_own() {
        let store = Store::open_in_memory().unwrap();
        store
            .ceremony_insert(
                &ceremony("mine", CeremonyPurpose::Assert, "operator", 1_000),
                1_000,
            )
            .unwrap();
        store.ceremony_prove("mine", "p1", 9_000).unwrap();
        for i in 0..50 {
            store
                .ceremony_insert(
                    &ceremony(
                        &format!("x{i}"),
                        CeremonyPurpose::Assert,
                        "attacker",
                        1_001 + i,
                    ),
                    1_001 + i,
                )
                .unwrap();
        }
        assert!(store.proof_spend("p1", "op-a", "operator", 1_500).unwrap());
    }

    #[test]
    fn rotating_the_token_keeps_passkeys_and_drops_ceremonies() {
        let store = Store::open_in_memory().unwrap();
        store.passkey_insert(&passkey("a"), true).unwrap();
        store
            .ceremony_insert(&ceremony("c1", CeremonyPurpose::Assert, "s1", 1_000), 1_000)
            .unwrap();
        store.ceremony_prove("c1", "p1", 9_000).unwrap();
        store.set_operator_token(Some(&"0".repeat(64))).unwrap();
        assert_eq!(store.passkey_count().unwrap(), 1);
        assert!(!store.proof_spend("p1", "op-a", "s1", 1_500).unwrap());
        store.set_operator_token(None).unwrap();
        assert_eq!(store.passkey_count().unwrap(), 1);
    }
}
