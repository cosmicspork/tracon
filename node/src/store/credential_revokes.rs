//! Durable state for revoking a removed credential across the mesh.
//!
//! A credential removed here was handed to the peers its `nodes` lists. Each
//! of them is owed a revoke, and the record says which have confirmed it: a
//! revoke queued at the hub is delivery progress, and only the peer's
//! authenticated receipt is confirmation. Delivery follows the policy
//! rollouts' rules (`mesh::client::redelivery_due`), except that nothing
//! supersedes a revoke: a target is resent until it answers, until
//! [`REVOKE_MAX_ATTEMPTS`] sends have gone unanswered, or until the operator
//! cancels the revoke.

use serde::Serialize;

use super::{now_ms, Result, Store};

/// How many times a revoke is queued for one peer before it is abandoned: a
/// day of resends, enough for a peer that is reachable to have answered.
/// One that cannot read the frame never will. A send the hub could not be
/// given counts too, such as one to a node that left the mesh and whose
/// sealing key is gone: it is tried at each reconcile, so such a peer is
/// abandoned after this many of those instead. A peer heard from again
/// after going quiet, or announcing another build, is sent its abandoned
/// revokes afresh.
pub const REVOKE_MAX_ATTEMPTS: i64 = 48;

/// One peer's revoke, with what it revokes.
#[derive(Debug, Clone, Serialize)]
pub struct CredentialRevokeTargetRow {
    pub revoke_id: String,
    pub name: String,
    pub grant_id: Option<String>,
    pub grant_version: i64,
    pub node_id: String,
    pub status: String,
    /// What the peer's receipt said it did: `dropped`, `kept` or `not_held`,
    /// or `unknown` for an answer from a newer peer that this node cannot
    /// read.
    pub outcome: Option<String>,
    pub detail: Option<String>,
    pub attempts: i64,
    pub last_sent_ms: Option<i64>,
    pub acknowledged_ms: Option<i64>,
    pub created_ms: i64,
}

impl CredentialRevokeTargetRow {
    fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(Self {
            revoke_id: row.get("revoke_id")?,
            name: row.get("name")?,
            grant_id: row.get("grant_id")?,
            grant_version: row.get("grant_version")?,
            node_id: row.get("node_id")?,
            status: row.get("status")?,
            outcome: row.get("outcome")?,
            detail: row.get("detail")?,
            attempts: row.get("attempts")?,
            last_sent_ms: row.get("last_sent_ms")?,
            acknowledged_ms: row.get("acknowledged_ms")?,
            created_ms: row.get("created_ms")?,
        })
    }

    /// Only the peer's receipt sets this. A peer that answered it kept its
    /// copy confirmed it as well: see [`Self::kept`].
    pub fn confirmed(&self) -> bool {
        self.status == "confirmed"
    }

    /// The peer answered that it holds the credential still, as another
    /// node's sign-in or its own.
    pub fn kept(&self) -> bool {
        self.confirmed() && self.outcome.as_deref() == Some("kept")
    }

    /// Sent [`REVOKE_MAX_ATTEMPTS`] times without an answer, and no longer.
    pub fn abandoned(&self) -> bool {
        self.status == "abandoned"
    }
}

const SELECT: &str = "SELECT t.revoke_id, r.name, r.grant_id, r.grant_version, t.node_id,
        t.status, t.outcome, t.detail, t.attempts, t.last_sent_ms, t.acknowledged_ms, r.created_ms
     FROM credential_revoke_target t JOIN credential_revoke r ON r.id = t.revoke_id";

impl Store {
    /// Record that `name`, of the given grant, is to be revoked from each of
    /// `targets`. Returns the revoke's id.
    pub fn create_credential_revoke(
        &self,
        name: &str,
        grant_id: Option<&str>,
        grant_version: u64,
        targets: &[String],
    ) -> Result<String> {
        let id = uuid::Uuid::now_v7().to_string();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO credential_revoke (id, name, grant_id, grant_version, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, name, grant_id, grant_version as i64, now_ms()],
        )?;
        for node_id in targets {
            tx.execute(
                "INSERT OR IGNORE INTO credential_revoke_target (revoke_id, node_id, status)
                 VALUES (?1, ?2, 'pending')",
                rusqlite::params![id, node_id],
            )?;
        }
        tx.commit()?;
        Ok(id)
    }

    /// Every peer of one revoke, by node id.
    pub fn credential_revoke_targets(
        &self,
        revoke_id: &str,
    ) -> Result<Vec<CredentialRevokeTargetRow>> {
        let conn = self.conn.lock().unwrap();
        let mut statement =
            conn.prepare(&format!("{SELECT} WHERE t.revoke_id=?1 ORDER BY t.node_id"))?;
        let rows = statement.query_map([revoke_id], CredentialRevokeTargetRow::from_row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Every revoke's peers, newest revoke first: what `tracon credential
    /// revokes` shows.
    pub fn credential_revokes(&self) -> Result<Vec<CredentialRevokeTargetRow>> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(&format!(
            "{SELECT} ORDER BY r.created_ms DESC, r.id DESC, t.node_id"
        ))?;
        let rows = statement.query_map([], CredentialRevokeTargetRow::from_row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Every unconfirmed revoke, or one peer's with `only`. What is due again
    /// is the caller's decision.
    pub fn credential_revoke_reconcile_targets(
        &self,
        only: Option<&str>,
    ) -> Result<Vec<CredentialRevokeTargetRow>> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(&format!(
            "{SELECT} WHERE t.status NOT IN ('confirmed','abandoned') AND (?1 IS NULL OR t.node_id = ?1)
             ORDER BY r.created_ms, t.node_id"
        ))?;
        let rows = statement.query_map([only], CredentialRevokeTargetRow::from_row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// The revoke is in the outbox, bound for the hub, which keeps it until
    /// the peer next pulls.
    pub fn mark_credential_revoke_sent(&self, revoke_id: &str, node_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "UPDATE credential_revoke_target
             SET status='sent', detail='Queued at the hub; delivered when the peer next connects. Not confirmed.',
                 attempts=attempts+1, last_sent_ms=?3
             WHERE revoke_id=?1 AND node_id=?2 AND status NOT IN ('confirmed','abandoned')",
            rusqlite::params![revoke_id, node_id, now_ms()],
        )? > 0)
    }

    /// The revoke could not be queued at all. It counts as a send toward
    /// [`REVOKE_MAX_ATTEMPTS`], so a peer nothing can be sealed to is not
    /// tried for good.
    pub fn mark_credential_revoke_offline(
        &self,
        revoke_id: &str,
        node_id: &str,
        detail: &str,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "UPDATE credential_revoke_target SET status='offline', detail=?3, attempts=attempts+1
             WHERE revoke_id=?1 AND node_id=?2 AND status NOT IN ('confirmed','abandoned')",
            rusqlite::params![revoke_id, node_id, detail],
        )? > 0)
    }

    /// Records only a receipt that names this revoke and its credential,
    /// with what the peer said it did. The caller supplies `node_id` from a
    /// verified envelope, never frame data. A peer answering after it was
    /// abandoned is recorded all the same.
    pub fn confirm_credential_revoke(
        &self,
        revoke_id: &str,
        node_id: &str,
        name: &str,
        outcome: Option<&str>,
    ) -> Result<bool> {
        let detail = match outcome {
            Some("dropped") => "Confirmed by the peer: its copy was dropped.",
            Some("kept") => "The peer kept its copy: another node's sign-in, or its own. Remove it there if it should go.",
            Some("not_held") => "Confirmed by the peer: it held nothing of that name.",
            Some("unknown") => "Confirmed by the peer, in words this node does not know: a newer peer's answer.",
            _ => "Confirmed by the peer.",
        };
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "UPDATE credential_revoke_target
             SET status='confirmed', outcome=?4, detail=?5, acknowledged_ms=?6
             WHERE revoke_id=?1 AND node_id=?2 AND status != 'confirmed'
               AND EXISTS (SELECT 1 FROM credential_revoke WHERE id=?1 AND name=?3)",
            rusqlite::params![revoke_id, node_id, name, outcome, detail, now_ms()],
        )? > 0)
    }

    /// Stop resending a revoke a peer has not answered in
    /// [`REVOKE_MAX_ATTEMPTS`] sends.
    pub fn abandon_credential_revoke(&self, revoke_id: &str, node_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            &format!(
                "UPDATE credential_revoke_target
                 SET status='abandoned',
                     detail='Not answered after {REVOKE_MAX_ATTEMPTS} sends or tries to send; sent again if the peer goes quiet and comes back, or runs another build. `tracon credential revokes --cancel` clears it.'
                 WHERE revoke_id=?1 AND node_id=?2 AND status NOT IN ('confirmed','abandoned')"
            ),
            rusqlite::params![revoke_id, node_id],
        )? > 0)
    }

    /// Send `node_id`'s abandoned revokes again, from a fresh count: the
    /// peer has just come back or announced another build, perhaps one that
    /// reads the frame it never answered. Returns how many were re-armed.
    pub fn rearm_credential_revokes(&self, node_id: &str) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "UPDATE credential_revoke_target
             SET status='pending', attempts=0,
                 detail='Abandoned, then sent again when the peer came back or changed builds.'
             WHERE node_id=?1 AND status='abandoned'",
            [node_id],
        )?)
    }

    /// Forget a revoke and every peer it names, so nothing more is sent: the
    /// operator's cancel, or a removal whose store could not be written.
    /// `id` may be the start of one revoke's id. Returns the ids removed;
    /// none when no revoke matches, and none when the start matches several,
    /// which [`Self::credential_revoke_ids`] tells apart.
    pub fn cancel_credential_revoke(&self, id: &str) -> Result<Vec<String>> {
        let ids = self.credential_revoke_ids(id)?;
        if ids.len() != 1 {
            return Ok(Vec::new());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM credential_revoke_target WHERE revoke_id=?1",
            [&ids[0]],
        )?;
        tx.execute("DELETE FROM credential_revoke WHERE id=?1", [&ids[0]])?;
        tx.commit()?;
        Ok(ids)
    }

    /// The revokes whose id starts with `prefix`.
    pub fn credential_revoke_ids(&self, prefix: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(
            "SELECT id FROM credential_revoke WHERE substr(id, 1, length(?1)) = ?1 ORDER BY id",
        )?;
        let rows = statement.query_map([prefix], |row| row.get(0))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_of(store: &Store, revoke_id: &str, node_id: &str) -> String {
        store
            .credential_revoke_targets(revoke_id)
            .unwrap()
            .into_iter()
            .find(|t| t.node_id == node_id)
            .unwrap()
            .status
    }

    #[test]
    fn a_revoke_is_confirmed_only_by_its_own_peer_naming_its_credential() {
        let store = Store::open_in_memory().unwrap();
        let id = store
            .create_credential_revoke("glab", Some("0190-a"), 2, &["a".into(), "b".into()])
            .unwrap();
        assert_eq!(status_of(&store, &id, "a"), "pending");
        assert!(store.mark_credential_revoke_sent(&id, "a").unwrap());

        assert!(!store
            .confirm_credential_revoke(&id, "a", "other", Some("dropped"))
            .unwrap());
        assert!(!store
            .confirm_credential_revoke(&id, "c", "glab", Some("dropped"))
            .unwrap());
        assert!(store
            .confirm_credential_revoke(&id, "a", "glab", Some("dropped"))
            .unwrap());
        assert_eq!(status_of(&store, &id, "a"), "confirmed");
        // Late bookkeeping cannot unconfirm it.
        assert!(!store.mark_credential_revoke_sent(&id, "a").unwrap());
        assert!(!store
            .mark_credential_revoke_offline(&id, "a", "gone")
            .unwrap());

        let open: Vec<_> = store
            .credential_revoke_reconcile_targets(None)
            .unwrap()
            .into_iter()
            .map(|t| (t.node_id, t.name, t.grant_id, t.grant_version))
            .collect();
        assert_eq!(
            open,
            vec![("b".into(), "glab".into(), Some("0190-a".into()), 2)]
        );
        assert!(store
            .credential_revoke_reconcile_targets(Some("a"))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_peer_that_kept_its_copy_is_recorded_as_keeping_it() {
        let store = Store::open_in_memory().unwrap();
        let id = store
            .create_credential_revoke("glab", Some("0190-a"), 0, &["d".into(), "e".into()])
            .unwrap();
        store
            .confirm_credential_revoke(&id, "d", "glab", Some("kept"))
            .unwrap();
        store
            .confirm_credential_revoke(&id, "e", "glab", Some("dropped"))
            .unwrap();
        let targets = store.credential_revoke_targets(&id).unwrap();
        assert!(targets[0].confirmed() && targets[0].kept());
        assert!(targets[0]
            .detail
            .as_deref()
            .unwrap()
            .contains("kept its copy"));
        assert!(targets[1].confirmed() && !targets[1].kept());
        assert_eq!(targets[1].outcome.as_deref(), Some("dropped"));
    }

    /// Second review of #608, nit 5: a newer peer's outcome this build does
    /// not know still confirms the revoke, and is kept as unknown.
    #[test]
    fn a_receipt_with_an_unknown_outcome_confirms_the_revoke() {
        let store = Store::open_in_memory().unwrap();
        let id = store
            .create_credential_revoke("glab", Some("0190-a"), 0, &["d".into()])
            .unwrap();
        assert!(store
            .confirm_credential_revoke(&id, "d", "glab", Some("unknown"))
            .unwrap());
        let target = store.credential_revoke_targets(&id).unwrap().remove(0);
        assert!(target.confirmed() && !target.kept());
        assert_eq!(target.outcome.as_deref(), Some("unknown"));
    }

    /// Second review of #608, nit 4: a revoke that could not be queued at
    /// all counts toward the sends it is allowed.
    #[test]
    fn a_revoke_that_could_not_be_queued_counts_as_an_attempt() {
        let store = Store::open_in_memory().unwrap();
        let id = store
            .create_credential_revoke("glab", None, 0, &["gone".into()])
            .unwrap();
        for _ in 0..3 {
            assert!(store
                .mark_credential_revoke_offline(&id, "gone", "no sealing key known")
                .unwrap());
        }
        let target = store.credential_revoke_targets(&id).unwrap().remove(0);
        assert_eq!((target.status.as_str(), target.attempts), ("offline", 3));
    }

    #[test]
    fn an_abandoned_revoke_is_not_resent_and_a_cancelled_one_is_gone() {
        let store = Store::open_in_memory().unwrap();
        let id = store
            .create_credential_revoke("glab", None, 0, &["a".into(), "b".into()])
            .unwrap();
        assert!(store.abandon_credential_revoke(&id, "a").unwrap());
        assert_eq!(status_of(&store, &id, "a"), "abandoned");
        assert!(!store.mark_credential_revoke_sent(&id, "a").unwrap());
        let open: Vec<_> = store
            .credential_revoke_reconcile_targets(None)
            .unwrap()
            .into_iter()
            .map(|t| t.node_id)
            .collect();
        assert_eq!(open, ["b"]);

        // A cancel takes a revoke by the start of its id, and only one.
        let other = store
            .create_credential_revoke("jira", None, 0, &["a".into()])
            .unwrap();
        let shared = id
            .chars()
            .zip(other.chars())
            .take_while(|(x, y)| x == y)
            .count();
        assert!(store
            .cancel_credential_revoke(&id[..shared])
            .unwrap()
            .is_empty());
        assert_eq!(
            store.cancel_credential_revoke(&id).unwrap(),
            std::slice::from_ref(&id)
        );
        assert!(store.credential_revoke_targets(&id).unwrap().is_empty());
        assert!(store.cancel_credential_revoke(&id).unwrap().is_empty());
        assert_eq!(store.credential_revokes().unwrap().len(), 1);
    }
}
