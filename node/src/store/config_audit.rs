//! Every write of `node.toml`: who made it, the file's hash before and after,
//! and the keys it changed. Written by `config::edit` before the file is
//! replaced, so no write reaches the file without a row here.

use rusqlite::params;
use serde::Serialize;

use super::{Result, Store};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConfigAuditRow {
    pub id: String,
    pub at_ms: i64,
    pub actor: String,
    pub old_hash: String,
    pub new_hash: String,
    /// `[{key, before, after}]`, a secret key's values replaced by
    /// `config::edit::REDACTED`.
    pub changes: serde_json::Value,
}

impl Store {
    pub fn config_audit_insert(&self, row: &ConfigAuditRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO config_audit (id, at_ms, actor, old_hash, new_hash, changes_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                row.id,
                row.at_ms,
                row.actor,
                row.old_hash,
                row.new_hash,
                row.changes.to_string()
            ],
        )?;
        Ok(())
    }

    /// Withdraw a row whose write then failed, so the log holds only what
    /// reached the file.
    pub fn config_audit_delete(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM config_audit WHERE id = ?1", [id])?;
        Ok(())
    }

    /// The newest `limit` rows, newest first.
    pub fn config_audit_recent(&self, limit: i64) -> Result<Vec<ConfigAuditRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, at_ms, actor, old_hash, new_hash, changes_json FROM config_audit
             ORDER BY at_ms DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([limit], |r| {
                let changes: String = r.get(5)?;
                Ok(ConfigAuditRow {
                    id: r.get(0)?,
                    at_ms: r.get(1)?,
                    actor: r.get(2)?,
                    old_hash: r.get(3)?,
                    new_hash: r.get(4)?,
                    changes: serde_json::from_str(&changes).unwrap_or_default(),
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }
}
