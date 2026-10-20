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
        // Whether or not a node is among the new targets, once a newer bundle
        // is installed here an older one must never be pushed again.
        supersede_older(
            &tx,
            Some(&rollout.id),
            &format!(
                "Superseded by rollout {} (v{}).",
                rollout.id, rollout.policy_version
            ),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Stop every unconfirmed delivery of an older rollout after a bundle was
    /// installed here without one (a local apply that selected no targets).
    pub fn supersede_policy_rollouts(&self, detail: &str) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        supersede_older(&conn, None, detail)
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
             WHERE rollout_id=?1 AND status NOT IN ('applied','superseded') ORDER BY node_id",
        )?;
        let rows = statement.query_map([rollout_id], PolicyRolloutTargetRow::from_row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Each node's row in the newest rollout that still speaks for it: the
    /// latest one that includes the node and has not been superseded. `only`
    /// narrows it to one node. What is due again is the caller's decision.
    pub fn policy_rollout_reconcile_targets(
        &self,
        only: Option<&str>,
    ) -> Result<Vec<PolicyRolloutTargetRow>> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(
            "SELECT t.rollout_id, t.node_id, t.status, t.detail, t.attempts, t.last_sent_ms,
                    t.acknowledged_ms
             FROM policy_rollout_target t JOIN policy_rollout r ON r.id = t.rollout_id
             WHERE t.status != 'superseded' AND (?1 IS NULL OR t.node_id = ?1)
               AND NOT EXISTS (
                   SELECT 1 FROM policy_rollout_target t2
                   JOIN policy_rollout r2 ON r2.id = t2.rollout_id
                   WHERE t2.node_id = t.node_id AND t2.status != 'superseded'
                     AND (r2.created_ms > r.created_ms
                          OR (r2.created_ms = r.created_ms AND r2.id > r.id)))
             ORDER BY t.node_id",
        )?;
        let rows = statement.query_map([only], PolicyRolloutTargetRow::from_row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// The bundle is in the outbox, bound for the hub, which keeps it until
    /// the peer next pulls. That is delivery progress, not installation.
    pub fn mark_policy_rollout_sent(&self, rollout_id: &str, node_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "UPDATE policy_rollout_target
             SET status='sent', detail='Queued at the hub; delivered when the peer next connects. Installation not confirmed.',
                 attempts=attempts+1, last_sent_ms=?3
             WHERE rollout_id=?1 AND node_id=?2 AND status NOT IN ('applied','superseded')",
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
             WHERE rollout_id=?1 AND node_id=?2 AND status NOT IN ('applied','superseded')",
            rusqlite::params![rollout_id, node_id, detail],
        )? > 0)
    }

    /// A peer's signed hello names the bundle it runs. When that is a
    /// rollout's exact bundle under the rollout's signing identity, the peer
    /// has it installed whether or not its receipt arrived. `node_id` is the
    /// hello's verified sender, never frame data.
    pub fn confirm_policy_rollout_by_hello(
        &self,
        node_id: &str,
        bundle_sha256: &str,
        identity: &str,
    ) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "UPDATE policy_rollout_target
             SET status='applied', detail='Confirmed by the peer''s signed hello.', acknowledged_ms=?4
             WHERE node_id=?1 AND status != 'applied'
               AND rollout_id IN (
                   SELECT id FROM policy_rollout
                   WHERE bundle_sha256=?2 AND lower(pubkey_hex)=lower(?3))",
            rusqlite::params![node_id, bundle_sha256, identity.trim(), now_ms()],
        )?)
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

/// Mark every unconfirmed target of every rollout but `keep` superseded.
fn supersede_older(conn: &rusqlite::Connection, keep: Option<&str>, detail: &str) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE policy_rollout_target SET status='superseded', detail=?2
         WHERE (?1 IS NULL OR rollout_id != ?1) AND status NOT IN ('applied','superseded')",
        rusqlite::params![keep, detail],
    )?)
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

    fn rollout_at(id: &str, version: i64, created_ms: i64) -> PolicyRolloutRow {
        PolicyRolloutRow {
            id: id.into(),
            bundle_sha256: format!("hash-{id}"),
            policy_version: version,
            created_ms,
            updated_ms: created_ms,
            ..rollout()
        }
    }

    fn status_of(store: &Store, rollout_id: &str, node_id: &str) -> String {
        store
            .policy_rollout_targets(rollout_id)
            .unwrap()
            .into_iter()
            .find(|t| t.node_id == node_id)
            .unwrap()
            .status
    }

    #[test]
    fn the_superseded_status_migration_keeps_every_existing_target() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::store::schema::migrate_to(&conn, 67).unwrap();
        conn.execute_batch(
            "INSERT INTO policy_rollout (id, bundle_sha256, policy_version, toml, sig_hex,
                pubkey_hex, source_node, created_ms, updated_ms)
             VALUES ('r1', 'h', 7, 'version = 7', 's', 'p', 'me', 1, 1);
             INSERT INTO policy_rollout_target (rollout_id, node_id, status, detail, attempts,
                last_sent_ms, acknowledged_ms)
             VALUES ('r1', 'a', 'sent', 'queued', 2, 10, NULL),
                    ('r1', 'b', 'applied', NULL, 1, 5, 6);",
        )
        .unwrap();
        assert!(conn
            .execute(
                "UPDATE policy_rollout_target SET status='superseded' WHERE node_id='a'",
                [],
            )
            .is_err());

        crate::store::schema::migrate(&conn).unwrap();
        let rows: Vec<(String, String, i64, Option<i64>)> = conn
            .prepare(
                "SELECT node_id, status, attempts, last_sent_ms FROM policy_rollout_target
                 ORDER BY node_id",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                ("a".into(), "sent".into(), 2, Some(10)),
                ("b".into(), "applied".into(), 1, Some(5)),
            ]
        );
        conn.execute(
            "UPDATE policy_rollout_target SET status='superseded' WHERE node_id='a'",
            [],
        )
        .unwrap();
    }

    #[test]
    fn a_new_rollout_supersedes_older_unconfirmed_targets_but_not_applied_ones() {
        let store = Store::open_in_memory().unwrap();
        store
            .create_policy_rollout(
                &rollout_at("old", 7, 1),
                &["a".into(), "b".into(), "c".into()],
            )
            .unwrap();
        store
            .record_policy_receipt("old", "b", "hash-old", true, None)
            .unwrap();
        store.mark_policy_rollout_sent("old", "c").unwrap();
        // `c` is not among the new targets, and still never gets v7 again.
        store
            .create_policy_rollout(&rollout_at("new", 8, 2), &["a".into(), "b".into()])
            .unwrap();

        assert_eq!(status_of(&store, "old", "a"), "superseded");
        assert_eq!(status_of(&store, "old", "b"), "applied");
        assert_eq!(status_of(&store, "old", "c"), "superseded");
        let detail = store.policy_rollout_targets("old").unwrap()[0]
            .detail
            .clone();
        assert_eq!(detail.as_deref(), Some("Superseded by rollout new (v8)."));
        assert_eq!(status_of(&store, "new", "a"), "pending");
        // Late delivery bookkeeping cannot revive a superseded target.
        assert!(!store.mark_policy_rollout_sent("old", "a").unwrap());
        assert!(!store
            .mark_policy_rollout_offline("old", "c", "gone")
            .unwrap());
        assert_eq!(status_of(&store, "old", "c"), "superseded");
    }

    #[test]
    fn retry_targets_exclude_superseded_and_applied_ones() {
        let store = Store::open_in_memory().unwrap();
        store
            .create_policy_rollout(&rollout_at("old", 7, 1), &["a".into(), "b".into()])
            .unwrap();
        store
            .record_policy_receipt("old", "a", "hash-old", true, None)
            .unwrap();
        store
            .create_policy_rollout(&rollout_at("new", 8, 2), &["c".into()])
            .unwrap();
        assert!(store
            .policy_rollout_retry_targets("old")
            .unwrap()
            .is_empty());
        let new: Vec<_> = store
            .policy_rollout_retry_targets("new")
            .unwrap()
            .into_iter()
            .map(|t| t.node_id)
            .collect();
        assert_eq!(new, vec!["c"]);
    }

    #[test]
    fn reconcile_reads_each_nodes_row_in_its_newest_live_rollout() {
        let store = Store::open_in_memory().unwrap();
        store
            .create_policy_rollout(&rollout_at("old", 7, 1), &["a".into(), "b".into()])
            .unwrap();
        store
            .record_policy_receipt("old", "a", "hash-old", true, None)
            .unwrap();
        store
            .create_policy_rollout(&rollout_at("new", 8, 2), &["b".into()])
            .unwrap();
        let rows: Vec<_> = store
            .policy_rollout_reconcile_targets(None)
            .unwrap()
            .into_iter()
            .map(|t| (t.node_id, t.rollout_id, t.status))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("a".into(), "old".into(), "applied".into()),
                ("b".into(), "new".into(), "pending".into()),
            ]
        );
        let only = store.policy_rollout_reconcile_targets(Some("b")).unwrap();
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].rollout_id, "new");
    }

    #[test]
    fn a_hello_confirms_only_the_target_whose_bundle_and_identity_it_names() {
        let store = Store::open_in_memory().unwrap();
        store
            .create_policy_rollout(&rollout_at("r1", 7, 1), &["a".into(), "b".into()])
            .unwrap();
        store
            .create_policy_rollout(
                &PolicyRolloutRow {
                    pubkey_hex: "public-b".into(),
                    ..rollout_at("r2", 8, 2)
                },
                &["c".into()],
            )
            .unwrap();

        // Another node, another bundle, another signer: none of them match.
        assert_eq!(
            store
                .confirm_policy_rollout_by_hello("c", "hash-r1", "public-a")
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .confirm_policy_rollout_by_hello("a", "hash-r2", "public-a")
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .confirm_policy_rollout_by_hello("c", "hash-r2", "public-a")
                .unwrap(),
            0
        );

        assert_eq!(
            store
                .confirm_policy_rollout_by_hello("a", "hash-r1", "PUBLIC-A")
                .unwrap(),
            1
        );
        let a = &store.policy_rollout_targets("r1").unwrap()[0];
        assert_eq!(a.status, "applied");
        assert!(a.acknowledged_ms.is_some());
        assert_eq!(
            a.detail.as_deref(),
            Some("Confirmed by the peer's signed hello.")
        );
        assert_eq!(status_of(&store, "r1", "b"), "superseded");
        assert_eq!(status_of(&store, "r2", "c"), "pending");
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
