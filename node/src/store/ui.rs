//! Capabilities for the native UI's own origin.
//!
//! Nothing here is a credential for the node. A `ui_grant` row says only "this
//! browser may drive *this* session's harness API, on *this* origin, until
//! *this* moment" — and the gateway it reaches is the same deny-by-default one
//! the operator's own interface calls, so the authority a grant carries is
//! bounded by the route matrix rather than by what the row says.
//!
//! What is stored is a SHA-256, as everywhere else in this codebase: of the
//! boot token, and of the cookie. A read of the database mints neither.
//!
//! Two properties are the table's rather than a code path's:
//!
//! * **A boot token is used once.** The exchange is a conditional `UPDATE`
//!   guarded on `used_ms IS NULL`, so two browsers racing the same fragment
//!   produce one cookie and one refusal, whatever the handler does.
//! * **A cookie is only as live as what it was minted against.** The row keeps
//!   the session it is for and a fingerprint of the operator login that asked
//!   for it, and both are re-read on every request. Ending the session or
//!   logging the operator out therefore revokes the cookie without anything
//!   having to remember to delete it.

use rusqlite::{params, OptionalExtension};

use super::{Result, Store};

/// A single-use bootstrap token, handed to the browser in a URL fragment.
pub const UI_GRANT_BOOT: &str = "boot";
/// The cookie a boot token is exchanged for.
pub const UI_GRANT_COOKIE: &str = "cookie";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiGrantRow {
    pub token_hash: String,
    pub kind: String,
    /// The tracon session this grant drives. Not the harness's `ses_` id: the
    /// gateway resolves that from the running session, so a grant cannot name
    /// a harness endpoint.
    pub session_id: String,
    /// The UI origin the grant is valid on, exactly as the browser sends it.
    pub audience: String,
    /// SHA-256 of the operator session cookie that asked for this, or empty
    /// when the operator was on loopback and carried none.
    pub operator: String,
    pub created_ms: i64,
    pub expires_ms: i64,
    pub used_ms: Option<i64>,
}

fn row_from(r: &rusqlite::Row) -> rusqlite::Result<UiGrantRow> {
    Ok(UiGrantRow {
        token_hash: r.get("token_hash")?,
        kind: r.get("kind")?,
        session_id: r.get("session_id")?,
        audience: r.get("audience")?,
        operator: r.get("operator")?,
        created_ms: r.get("created_ms")?,
        expires_ms: r.get("expires_ms")?,
        used_ms: r.get("used_ms")?,
    })
}

impl Store {
    pub fn ui_grant_insert(&self, row: &UiGrantRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO ui_grant
                (token_hash, kind, session_id, audience, operator, created_ms, expires_ms, used_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                row.token_hash,
                row.kind,
                row.session_id,
                row.audience,
                row.operator,
                row.created_ms,
                row.expires_ms,
                row.used_ms,
            ],
        )?;
        Ok(())
    }

    /// Spend a boot token: return the grant it named, and only ever once.
    ///
    /// The single-use guarantee is the `used_ms IS NULL` in the `UPDATE`, not
    /// the `SELECT` that follows it — a second caller's `UPDATE` matches no
    /// row and it gets `None`, whether it arrived a millisecond later or on
    /// another thread. Expiry and audience are in the same predicate so a
    /// token that is refused is never marked spent, and a token for one origin
    /// cannot be redeemed on another.
    pub fn ui_grant_consume_boot(
        &self,
        token_hash: &str,
        audience: &str,
        now: i64,
    ) -> Result<Option<UiGrantRow>> {
        let conn = self.conn.lock().unwrap();
        let spent = conn.execute(
            "UPDATE ui_grant SET used_ms=?3
              WHERE token_hash=?1 AND kind='boot' AND used_ms IS NULL
                AND expires_ms > ?3 AND audience=?2",
            params![token_hash, audience, now],
        )?;
        if spent == 0 {
            return Ok(None);
        }
        let mut stmt = conn.prepare("SELECT * FROM ui_grant WHERE token_hash=?1")?;
        Ok(stmt.query_row([token_hash], row_from).optional()?)
    }

    /// The live cookie grant a request carries, if it is still one.
    ///
    /// Liveness here is only the row's own: expiry and audience. Whether the
    /// session still runs and the operator is still logged in is asked of
    /// those tables on every request (`http::ui::authorise`), because a
    /// revocation that depended on this row being deleted would be a
    /// revocation that a crash between the two writes could lose.
    pub fn ui_grant_cookie(
        &self,
        token_hash: &str,
        audience: &str,
        now: i64,
    ) -> Result<Option<UiGrantRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM ui_grant
              WHERE token_hash=?1 AND kind='cookie' AND audience=?2 AND expires_ms > ?3",
        )?;
        Ok(stmt
            .query_row(params![token_hash, audience, now], row_from)
            .optional()?)
    }

    /// Drop what a session's end made meaningless. The per-request check
    /// already refuses these; this is so the table does not grow a row per
    /// session that ever had a UI open.
    pub fn ui_grants_forget_session(&self, session_id: &str) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM ui_grant WHERE session_id=?1", [session_id])?)
    }

    /// Expired rows, and spent boot tokens. A spent token is kept until it
    /// would have expired anyway so that a replay in its own lifetime is
    /// refused as *spent* rather than as unknown.
    pub fn ui_grants_purge(&self, now: i64) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM ui_grant WHERE expires_ms <= ?1", [now])?)
    }

    /// Every live cookie grant for a session, for tests and for the operator's
    /// view of what is attached.
    pub fn ui_grants_for_session(&self, session_id: &str) -> Result<Vec<UiGrantRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT * FROM ui_grant WHERE session_id=?1 ORDER BY created_ms DESC")?;
        let rows = stmt.query_map([session_id], row_from)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}
