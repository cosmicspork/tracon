use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use super::{now_ms, Result, Store};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthorityGrantRow {
    pub id: String,
    pub action: String,
    pub verdict: String,
    pub target: String,
    pub channel: String,
    pub session_id: Option<String>,
    pub revision: Option<String>,
    pub expires_ms: Option<i64>,
    pub revoked_ms: Option<i64>,
    pub reason: String,
    pub created_ms: i64,
}

impl AuthorityGrantRow {
    fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: r.get("id")?,
            action: r.get("action")?,
            verdict: r.get("verdict")?,
            target: r.get("target")?,
            channel: r.get("channel")?,
            session_id: r.get("session_id")?,
            revision: r.get("revision")?,
            expires_ms: r.get("expires_ms")?,
            revoked_ms: r.get("revoked_ms")?,
            reason: r.get("reason")?,
            created_ms: r.get("created_ms")?,
        })
    }
}

impl Store {
    pub fn authority_grants(&self, include_revoked: bool) -> Result<Vec<AuthorityGrantRow>> {
        let conn = self.conn.lock().unwrap();
        let sql = if include_revoked {
            "SELECT * FROM authority_grant ORDER BY created_ms DESC"
        } else {
            "SELECT * FROM authority_grant WHERE revoked_ms IS NULL ORDER BY created_ms DESC"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([], AuthorityGrantRow::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn authority_grant_insert(&self, row: &AuthorityGrantRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO authority_grant (id, action, verdict, target, channel, session_id, revision, expires_ms, revoked_ms, reason, created_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,NULL,?9,?10)",
            rusqlite::params![row.id, row.action, row.verdict, row.target, row.channel, row.session_id,
                row.revision, row.expires_ms, row.reason, row.created_ms],
        )?;
        Ok(())
    }

    pub fn authority_grant_revoke(&self, id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "UPDATE authority_grant SET revoked_ms=?2 WHERE id=?1 AND revoked_ms IS NULL",
            rusqlite::params![id, now_ms()],
        )? == 1)
    }

    pub fn authority_grants_for(
        &self,
        channel: &str,
        session_id: &str,
        action: &str,
        target: &str,
        revision: Option<&str>,
        at_ms: i64,
    ) -> Result<Vec<AuthorityGrantRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM authority_grant
             WHERE action=?1 AND target=?2 AND channel=?3
               AND (session_id IS NULL OR session_id=?4)
               AND (revision IS NULL OR revision=?5)
               AND revoked_ms IS NULL
               AND (expires_ms IS NULL OR expires_ms > ?6)
             ORDER BY created_ms DESC",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![action, target, channel, session_id, revision, at_ms],
            AuthorityGrantRow::from_row,
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn authority_grant(&self, id: &str) -> Result<Option<AuthorityGrantRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT * FROM authority_grant WHERE id=?1", [id], AuthorityGrantRow::from_row)
            .optional()
            .map_err(Into::into)
    }

    /// Create an outcome before dispatch. A crash after the remote side effect
    /// remains honestly pending rather than being retried blindly on restart.
    pub fn authority_action_begin(&self, id: &str, grant_id: Option<&str>, action: &str, target: &str, channel: &str, session_id: &str, revision: Option<&str>, operation_id: Option<&str>, evidence: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let request_hash = crate::corpus::hash_body(evidence);
        conn.execute(
            "INSERT INTO authority_action (id, grant_id, action, target, channel, session_id, revision, operation_id, evidence, state, outcome, created_ms, updated_ms, request_hash)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,'pending',NULL,?10,?10,?11)",
            rusqlite::params![id, grant_id, action, target, channel, session_id, revision, operation_id.unwrap_or(""), evidence, now_ms(), request_hash],
        )?;
        Ok(())
    }

    pub fn authority_action_existing(
        &self,
        action: &str,
        target: &str,
        channel: &str,
        revision: Option<&str>,
        operation_id: &str,
    ) -> Result<Option<(String, Option<String>, String)>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT state, outcome, request_hash FROM authority_action
             WHERE action=?1 AND target=?2 AND channel=?3
               AND revision IS ?4 AND operation_id=?5
               AND state IN ('pending', 'uncertain', 'succeeded')",
            rusqlite::params![action, target, channel, revision, operation_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(Into::into)
    }

    /// Attribute a pending dispatch to the authority decision that survived
    /// the final, immediately-before-send recheck.
    pub fn authority_action_set_grant(&self, id: &str, grant_id: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE authority_action SET grant_id=?2, updated_ms=?3 WHERE id=?1 AND state='pending'",
            rusqlite::params![id, grant_id, now_ms()],
        )?;
        Ok(())
    }

    pub fn authority_action_finish(&self, id: &str, state: &str, outcome: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE authority_action SET state=?2, outcome=?3, updated_ms=?4 WHERE id=?1 AND state='pending'",
            rusqlite::params![id, state, outcome, now_ms()],
        )?;
        Ok(())
    }
}
