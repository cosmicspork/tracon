//! What a publication did outside this node.
//!
//! A push and an opened merge request are side effects the node cannot take
//! back, so the record of each is written *before* it is attempted and
//! completed after. That ordering is the whole point: a crash in between
//! leaves a row that says "this may have happened", which is the truth, and
//! recovery then asks the forge what actually landed instead of pushing again
//! or reporting a failure that did not happen.
//!
//! States:
//!
//! - `pending` — nothing has been attempted yet, or the attempt is in flight.
//! - `pushed` — the branch was pushed and the remote was observed to hold the
//!   reviewed commit.
//! - `opening` — the forge CLI was about to be asked to open the change.
//! - `opened` — it is open, and `result` is where.
//! - `failed` — the attempt stopped somewhere no side effect could have
//!   landed, or the forge refused outright. Retryable.
//! - `uncertain` — the outcome is genuinely unknown: the node restarted
//!   mid-attempt, or the forge could not be reached to establish what is
//!   there. Never presented as success or failure.

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::{now_ms, Result, Store};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicationRow {
    pub id: String,
    pub review_id: String,
    pub revision_id: Option<String>,
    pub candidate_id: String,
    pub channel: String,
    pub node_id: String,
    pub provider: String,
    pub project: String,
    pub base: String,
    pub branch: String,
    pub head_sha: String,
    pub state: String,
    pub pushed_sha: Option<String>,
    pub result: Option<String>,
    pub note: Option<String>,
    pub attempts: i64,
    pub instance: String,
    pub created_ms: i64,
    pub updated_ms: i64,
}

impl PublicationRow {
    fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: r.get("id")?,
            review_id: r.get("review_id")?,
            revision_id: r.get("revision_id")?,
            candidate_id: r.get("candidate_id")?,
            channel: r.get("channel")?,
            node_id: r.get("node_id")?,
            provider: r.get("provider")?,
            project: r.get("project")?,
            base: r.get("base")?,
            branch: r.get("branch")?,
            head_sha: r.get("head_sha")?,
            state: r.get("state")?,
            pushed_sha: r.get("pushed_sha")?,
            result: r.get("result")?,
            note: r.get("note")?,
            attempts: r.get("attempts")?,
            instance: r.get("instance")?,
            created_ms: r.get("created_ms")?,
            updated_ms: r.get("updated_ms")?,
        })
    }

    /// Whether anything may already exist on the forge because of this row.
    /// `pending` counts: the crash could have landed on either side of the
    /// push, which is exactly why recovery looks before it acts.
    pub fn may_have_reached_the_forge(&self) -> bool {
        self.state != "failed" || self.pushed_sha.is_some()
    }
}

/// What a publication attempt is about to do, recorded before it does it.
pub struct PublicationBegin<'a> {
    pub id: &'a str,
    pub review_id: &'a str,
    pub revision_id: Option<&'a str>,
    pub candidate_id: &'a str,
    pub channel: &'a str,
    pub node_id: &'a str,
    pub provider: &'a str,
    pub project: &'a str,
    pub base: &'a str,
    pub branch: &'a str,
    pub head_sha: &'a str,
    pub instance: &'a str,
}

impl Store {
    /// Open (or reopen) the record for one publication. The row exists before
    /// the first side effect is attempted; a retry increments `attempts` and
    /// keeps everything the earlier attempt observed.
    pub fn publication_begin(&self, begin: &PublicationBegin<'_>) -> Result<PublicationRow> {
        {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO publication
                    (id, review_id, revision_id, candidate_id, channel, node_id, provider, project,
                     base, branch, head_sha, state, attempts, instance, created_ms, updated_ms)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,'pending',1,?12,?13,?13)
                 ON CONFLICT(id) DO UPDATE SET
                    state=CASE WHEN publication.state='opened' THEN 'opened' ELSE 'pending' END,
                    attempts=publication.attempts + 1,
                    instance=excluded.instance,
                    updated_ms=excluded.updated_ms",
                params![
                    begin.id,
                    begin.review_id,
                    begin.revision_id,
                    begin.candidate_id,
                    begin.channel,
                    begin.node_id,
                    begin.provider,
                    begin.project,
                    begin.base,
                    begin.branch,
                    begin.head_sha,
                    begin.instance,
                    now_ms(),
                ],
            )?;
        }
        self.publication(begin.id)?
            .ok_or_else(|| super::StoreError::Invalid("publication record vanished".into()))
    }

    pub fn publication(&self, id: &str) -> Result<Option<PublicationRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT * FROM publication WHERE id=?1",
            [id],
            PublicationRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Every publication record for a review, newest first, so the operator
    /// sees an interrupted attempt beside the review it belongs to.
    pub fn publications_for_review(&self, review_id: &str) -> Result<Vec<PublicationRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT * FROM publication WHERE review_id=?1 ORDER BY created_ms DESC")?;
        let rows = stmt
            .query_map([review_id], PublicationRow::from_row)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// The branch was pushed and the remote was then observed to hold it.
    pub fn publication_pushed(&self, id: &str, sha: &str) -> Result<()> {
        self.publication_set(id, "pushed", Some(sha), None, None)
    }

    /// About to ask the forge to open the change. Written first, so a crash
    /// during the call is visible as "an open may exist".
    pub fn publication_opening(&self, id: &str) -> Result<()> {
        self.publication_set(id, "opening", None, None, None)
    }

    pub fn publication_opened(&self, id: &str, result: &str) -> Result<()> {
        self.publication_set(id, "opened", None, Some(result), None)
    }

    pub fn publication_failed(&self, id: &str, note: &str) -> Result<()> {
        self.publication_set(id, "failed", None, None, Some(note))
    }

    /// The honest state when the forge could not be reached: not success, not
    /// failure, and never retried blindly.
    pub fn publication_uncertain(&self, id: &str, note: &str) -> Result<()> {
        self.publication_set(id, "uncertain", None, None, Some(note))
    }

    fn publication_set(
        &self,
        id: &str,
        state: &str,
        pushed_sha: Option<&str>,
        result: Option<&str>,
        note: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE publication
                SET state=?2,
                    pushed_sha=COALESCE(?3, pushed_sha),
                    result=COALESCE(?4, result),
                    note=?5,
                    updated_ms=?6
              WHERE id=?1",
            params![id, state, pushed_sha, result, note, now_ms()],
        )?;
        Ok(())
    }

    /// A review left mid-publish with no publication record to show for it
    /// was interrupted before the attempt could write down what it was about
    /// to do — and that write comes before anything outside this node. So
    /// nothing external can have happened, and the review belongs back in the
    /// queue rather than sitting in `publishing` with no way out.
    ///
    /// Only safe at startup, when no publish of this node's is in flight.
    pub fn reconcile_publishing_without_record(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "UPDATE review SET state='claimed', updated_ms=?1
              WHERE state='publishing'
                AND id NOT IN (SELECT review_id FROM publication)",
            params![now_ms()],
        )?)
    }

    /// Any attempt this process did not start is one a previous process left
    /// in flight: the node went down between writing the record and learning
    /// the outcome. Relabel it `uncertain` rather than leaving it looking
    /// live, and leave the evidence (`pushed_sha`, `result`) alone.
    pub fn publication_reconcile_interrupted(&self, instance: &str) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "UPDATE publication
                SET state='uncertain',
                    note=COALESCE(note, 'the node restarted before this publication reported its outcome'),
                    updated_ms=?2
              WHERE state IN ('pending','opening') AND instance <> ?1",
            params![instance, now_ms()],
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn begin<'a>(id: &'a str, instance: &'a str) -> PublicationBegin<'a> {
        PublicationBegin {
            id,
            review_id: "r1",
            revision_id: Some("rev1"),
            candidate_id: "c1",
            channel: "work",
            node_id: "n1",
            provider: "github",
            project: "owner/name",
            base: "main",
            branch: "feat/x",
            head_sha: "abc",
            instance,
        }
    }

    #[test]
    fn a_retry_keeps_the_row_and_what_the_last_attempt_observed() {
        let store = Store::open_in_memory().unwrap();
        let row = store.publication_begin(&begin("p1", "one")).unwrap();
        assert_eq!(row.state, "pending");
        assert_eq!(row.attempts, 1);
        store.publication_pushed("p1", "abc").unwrap();

        let again = store.publication_begin(&begin("p1", "two")).unwrap();
        assert_eq!(again.attempts, 2);
        assert_eq!(again.instance, "two");
        assert_eq!(
            again.pushed_sha.as_deref(),
            Some("abc"),
            "a retry must not forget that the push landed"
        );
    }

    #[test]
    fn an_attempt_another_process_left_in_flight_becomes_uncertain() {
        let store = Store::open_in_memory().unwrap();
        store.publication_begin(&begin("p1", "before")).unwrap();
        store.publication_opening("p1").unwrap();
        store.publication_begin(&begin("p2", "now")).unwrap();
        store.publication_pushed("p2", "abc").unwrap();

        assert_eq!(store.publication_reconcile_interrupted("now").unwrap(), 1);
        let interrupted = store.publication("p1").unwrap().unwrap();
        assert_eq!(interrupted.state, "uncertain");
        assert!(interrupted.note.unwrap().contains("restarted"));
        // A row that reached a settled state is left exactly as it was.
        assert_eq!(store.publication("p2").unwrap().unwrap().state, "pushed");
    }

    #[test]
    fn an_opened_publication_is_never_reopened_as_pending() {
        let store = Store::open_in_memory().unwrap();
        store.publication_begin(&begin("p1", "one")).unwrap();
        store
            .publication_opened("p1", "https://forge/pull/1")
            .unwrap();
        let again = store.publication_begin(&begin("p1", "two")).unwrap();
        assert_eq!(again.state, "opened");
        assert_eq!(again.result.as_deref(), Some("https://forge/pull/1"));
    }
}
