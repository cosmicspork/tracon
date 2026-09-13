//! Durable operator-visible state for policy distribution.
//!
//! A hub accepting a ciphertext is only delivery progress. The durable target
//! record distinguishes that from an authenticated receiver receipt, so a UI
//! cannot accidentally present a queued bundle as installed.

use rusqlite::OptionalExtension;
use serde::Serialize;

use super::{now_ms, Result, Store};

#[derive(Debug, Clone, Serialize)]
pub struct PolicyRolloutRow {
    pub id: String,
    pub bundle_sha256: String,
    pub policy_version: i64,
    pub toml: String,
    pub sig_hex: String,
    pub pubkey_hex: String,
    pub source_node: String,
    pub created_ms: i64,
    pub updated_ms: i64,
}

impl PolicyRolloutRow {
    fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            bundle_sha256: row.get("bundle_sha256")?,
            policy_version: row.get("policy_version")?,
            toml: row.get("toml")?,
            sig_hex: row.get("sig_hex")?,
            pubkey_hex: row.get("pubkey_hex")?,
            source_node: row.get("source_node")?,
            created_ms: row.get("created_ms")?,
            updated_ms: row.get("updated_ms")?,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PolicyRolloutTargetRow {
    pub rollout_id: String,
    pub node_id: String,
    pub status: String,
    pub detail: Option<String>,
    pub attempts: i64,
    pub last_sent_ms: Option<i64>,
    pub acknowledged_ms: Option<i64>,
}

impl PolicyRolloutTargetRow {
    fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(Self {
            rollout_id: row.get("rollout_id")?,
            node_id: row.get("node_id")?,
            status: row.get("status")?,
            detail: row.get("detail")?,
            attempts: row.get("attempts")?,
            last_sent_ms: row.get("last_sent_ms")?,
            acknowledged_ms: row.get("acknowledged_ms")?,
        })
    }

    /// Delivery can be known while installation cannot. Only an authenticated
    /// receipt sets this true.
    pub fn confirmed(&self) -> bool {
        self.status == "applied"
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PolicyInstallationRow {
    pub bundle_sha256: String,
    pub source_node: Option<String>,
    pub rollout_id: Option<String>,
    pub applied_ms: i64,
}

impl PolicyInstallationRow {
    fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(Self {
            bundle_sha256: row.get("bundle_sha256")?,
            source_node: row.get("source_node")?,
            rollout_id: row.get("rollout_id")?,
            applied_ms: row.get("applied_ms")?,
        })
    }
}

impl Store {
    pub fn create_policy_rollout(
        &self,
        rollout: &PolicyRolloutRow,
        targets: &[String],
    ) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO policy_rollout
                (id, bundle_sha256, policy_version, toml, sig_hex, pubkey_hex, source_node, created_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                rollout.id,
                rollout.bundle_sha256,
                rollout.policy_version,
                rollout.toml,
                rollout.sig_hex,
                rollout.pubkey_hex,
                rollout.source_node,
                rollout.created_ms,
                rollout.updated_ms,
            ],
        )?;
        for node_id in targets {
            tx.execute(
                "INSERT OR IGNORE INTO policy_rollout_target
                    (rollout_id, node_id, status, attempts)
                 VALUES (?1, ?2, 'pending', 0)",
                rusqlite::params![rollout.id, node_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn policy_rollouts(&self) -> Result<Vec<PolicyRolloutRow>> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(
            "SELECT id, bundle_sha256, policy_version, toml, sig_hex, pubkey_hex, source_node, created_ms, updated_ms
             FROM policy_rollout ORDER BY created_ms DESC, id DESC LIMIT 100",
        )?;
        let rows = statement.query_map([], PolicyRolloutRow::from_row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn policy_rollout(&self, id: &str) -> Result<Option<PolicyRolloutRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, bundle_sha256, policy_version, toml, sig_hex, pubkey_hex, source_node, created_ms, updated_ms
             FROM policy_rollout WHERE id=?1",
            [id],
            PolicyRolloutRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn policy_rollout_targets(&self, rollout_id: &str) -> Result<Vec<PolicyRolloutTargetRow>> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(
            "SELECT rollout_id, node_id, status, detail, attempts, last_sent_ms, acknowledged_ms
             FROM policy_rollout_target WHERE rollout_id=?1 ORDER BY node_id",
        )?;
        let rows = statement.query_map([rollout_id], PolicyRolloutTargetRow::from_row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn policy_rollout_retry_targets(
        &self,
        rollout_id: &str,
    ) -> Result<Vec<PolicyRolloutTargetRow>> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(
            "SELECT rollout_id, node_id, status, detail, attempts, last_sent_ms, acknowledged_ms
             FROM policy_rollout_target
             WHERE rollout_id=?1 AND status != 'applied' ORDER BY node_id",
        )?;
        let rows = statement.query_map([rollout_id], PolicyRolloutTargetRow::from_row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn mark_policy_rollout_sent(&self, rollout_id: &str, node_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "UPDATE policy_rollout_target
             SET status='sent', detail='Queued for encrypted direct delivery; installation is not confirmed.',
                 attempts=attempts+1, last_sent_ms=?3
             WHERE rollout_id=?1 AND node_id=?2 AND status != 'applied'",
            rusqlite::params![rollout_id, node_id, now_ms()],
        )? > 0)
    }

    pub fn mark_policy_rollout_offline(
        &self,
        rollout_id: &str,
        node_id: &str,
        detail: &str,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "UPDATE policy_rollout_target
             SET status='offline', detail=?3
             WHERE rollout_id=?1 AND node_id=?2 AND status != 'applied'",
            rusqlite::params![rollout_id, node_id, detail],
        )? > 0)
    }

    /// Records only a receipt that names this exact rollout and bundle. The
    /// caller supplies `node_id` from a verified envelope, never frame data.
    pub fn record_policy_receipt(
        &self,
        rollout_id: &str,
        node_id: &str,
        bundle_sha256: &str,
        applied: bool,
        detail: Option<&str>,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let status = if applied { "applied" } else { "rejected" };
        Ok(conn.execute(
            "UPDATE policy_rollout_target
             SET status=?4, detail=?5, acknowledged_ms=?6
             WHERE rollout_id=?1 AND node_id=?2
               AND EXISTS (SELECT 1 FROM policy_rollout WHERE id=?1 AND bundle_sha256=?3)",
            rusqlite::params![rollout_id, node_id, bundle_sha256, status, detail, now_ms()],
        )? > 0)
    }

    pub fn policy_installation(&self) -> Result<Option<PolicyInstallationRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT bundle_sha256, source_node, rollout_id, applied_ms
             FROM policy_installation WHERE id=1",
            [],
            PolicyInstallationRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// The envelope's verified key is written as `source_node` for remote
    /// installs. This is provenance, not an authorization input: signature
    /// verification remains the decision that permits installation.
    pub fn record_policy_installation(
        &self,
        bundle_sha256: &str,
        source_node: Option<&str>,
        rollout_id: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO policy_installation (id, bundle_sha256, source_node, rollout_id, applied_ms)
             VALUES (1, ?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET bundle_sha256=?1, source_node=?2,
                 rollout_id=?3, applied_ms=?4",
            rusqlite::params![bundle_sha256, source_node, rollout_id, now_ms()],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rollout() -> PolicyRolloutRow {
        PolicyRolloutRow {
            id: "rollout-a".into(),
            bundle_sha256: "hash-a".into(),
            policy_version: 7,
            toml: "version = 7\n".into(),
            sig_hex: "signature-a".into(),
            pubkey_hex: "public-a".into(),
            source_node: "source-a".into(),
            created_ms: 1,
            updated_ms: 1,
        }
    }

    #[test]
    fn receipts_require_the_selected_authenticated_sender_and_exact_bundle() {
        let store = Store::open_in_memory().unwrap();
        let rollout = rollout();
        store
            .create_policy_rollout(&rollout, &["node-a".into()])
            .unwrap();

        assert!(!store
            .record_policy_receipt("rollout-a", "node-b", "hash-a", true, None)
            .unwrap());
        assert!(!store
            .record_policy_receipt("rollout-a", "node-a", "hash-b", true, None)
            .unwrap());
        assert!(!store
            .record_policy_receipt("rollout-b", "node-a", "hash-a", true, None)
            .unwrap());
        assert_eq!(
            store.policy_rollout_targets("rollout-a").unwrap()[0].status,
            "pending"
        );

        assert!(store
            .record_policy_receipt("rollout-a", "node-a", "hash-a", true, None)
            .unwrap());
        assert_eq!(
            store.policy_rollout_targets("rollout-a").unwrap()[0].status,
            "applied"
        );
    }

    #[test]
    fn retry_uses_the_immutable_bundle_retained_with_the_rollout() {
        let store = Store::open_in_memory().unwrap();
        let rollout = rollout();
        store
            .create_policy_rollout(&rollout, &["node-a".into()])
            .unwrap();

        let retained = store.policy_rollout("rollout-a").unwrap().unwrap();
        assert_eq!(retained.toml, "version = 7\n");
        assert_eq!(retained.sig_hex, "signature-a");
        assert_eq!(retained.pubkey_hex, "public-a");
        assert_eq!(retained.bundle_sha256, "hash-a");
        assert_eq!(
            store.policy_rollout_retry_targets("rollout-a").unwrap()[0].node_id,
            "node-a"
        );
    }
}
