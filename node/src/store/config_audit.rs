//! Every write of `node.toml`: who made it, the file's hash before and after,
//! and the keys it changed. Written by `config::edit` before the file is
//! replaced, as `pending`, and marked `landed` once it is, so no write reaches
//! the file without a row here and a row never claims a write that did not.

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
    /// `pending`, `landed` or `lost`.
    pub state: String,
}

impl Store {
    pub fn config_audit_insert(&self, row: &ConfigAuditRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO config_audit (id, at_ms, actor, old_hash, new_hash, changes_json, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending')",
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

    /// The write the row describes is in the file.
    pub fn config_audit_landed(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE config_audit SET state = 'landed' WHERE id = ?1",
            [id],
        )?;
        Ok(())
    }

    /// Settle every pending row. One whose new hash is a version the file
    /// has been in since (`versions`, or what a later landed write started
    /// from) landed; any other did not. Returns how many were settled.
    pub fn config_audit_settle(&self, versions: &[&str]) -> Result<usize> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let mut settled = 0;
        for version in versions {
            settled += tx.execute(
                "UPDATE config_audit SET state = 'landed' WHERE state = 'pending' AND new_hash = ?1",
                [version],
            )?;
        }
        loop {
            let built_on = tx.execute(
                "UPDATE config_audit SET state = 'landed'
                 WHERE state = 'pending' AND new_hash IN
                   (SELECT old_hash FROM config_audit WHERE state = 'landed')",
                [],
            )?;
            if built_on == 0 {
                break;
            }
            settled += built_on;
        }
        settled += tx.execute(
            "UPDATE config_audit SET state = 'lost' WHERE state = 'pending'",
            [],
        )?;
        tx.commit()?;
        Ok(settled)
    }

    /// The newest `limit` rows, newest first.
    pub fn config_audit_recent(&self, limit: i64) -> Result<Vec<ConfigAuditRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, at_ms, actor, old_hash, new_hash, changes_json, state FROM config_audit
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
                    state: r.get(6)?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }
}
