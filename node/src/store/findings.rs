//! What a review session found when it checked one criterion against the
//! candidate (`review_finding`).
//!
//! One row is one reviewer saying, of one criterion and one candidate, whether
//! what it ran supported it, contradicted it, or could not show it either way.
//! Append-only, like a criterion verdict: a later finding on the same criterion
//! and candidate supersedes the earlier one, and both are kept.
//!
//! Every row is `inferred`. It is a session's account, with the work it showed
//! as its evidence, and it never makes a criterion met: that word is only ever
//! written by the operator, in `store::criteria`. A finding can only raise a
//! criterion to "verified · unjudged".
//!
//! Node-local, like the checks and the shown work it cites: a peer mirroring
//! the review never saw the candidate run.

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::{now_ms, Result, ReviewRevisionRow, SessionRow, Store, StoreError};

/// What a reviewer may say of a criterion. `not_shown` is a finding too: the
/// reviewer looked and could not make the candidate show it either way.
pub const OUTCOMES: &[&str] = &["supported", "contradicted", "not_shown"];

/// The only standing a finding has: a session reasoned to it.
pub const PROVENANCE: &str = "inferred";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFindingRow {
    pub id: String,
    pub channel: String,
    pub review_id: String,
    /// The candidate the reviewer exercised: the review's revision at the time.
    pub candidate_id: String,
    pub revision_id: Option<String>,
    pub work_item_id: String,
    pub brief_slug: String,
    /// The criterion's name, derived from its text by `corpus::criteria`.
    pub criterion_key: String,
    /// The criterion as it read when the finding was made.
    pub criterion_text: String,
    /// One of [`OUTCOMES`].
    pub outcome: String,
    pub note: String,
    /// The `show_work` ids the finding rests on.
    pub evidence: Vec<String>,
    /// Always [`PROVENANCE`].
    pub provenance: String,
    pub session_id: String,
    pub model: Option<String>,
    pub created_ms: i64,
}

impl ReviewFindingRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let evidence: String = row.get("evidence_json")?;
        Ok(Self {
            id: row.get("id")?,
            channel: row.get("channel")?,
            review_id: row.get("review_id")?,
            candidate_id: row.get("candidate_id")?,
            revision_id: row.get("revision_id")?,
            work_item_id: row.get("work_item_id")?,
            brief_slug: row.get("brief_slug")?,
            criterion_key: row.get("criterion_key")?,
            criterion_text: row.get("criterion_text")?,
            outcome: row.get("outcome")?,
            note: row.get("note")?,
            evidence: serde_json::from_str(&evidence).unwrap_or_default(),
            provenance: row.get("provenance")?,
            session_id: row.get("session_id")?,
            model: row.get("model")?,
            created_ms: row.get("created_ms")?,
        })
    }
}

impl Store {
    pub fn review_finding_record(&self, row: &ReviewFindingRow) -> Result<()> {
        let evidence = serde_json::to_string(&row.evidence)
            .map_err(|error| StoreError::Invalid(error.to_string()))?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO review_finding
                (id, channel, review_id, candidate_id, revision_id, work_item_id, brief_slug,
                 criterion_key, criterion_text, outcome, note, evidence_json, provenance,
                 session_id, model, created_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
            params![
                row.id,
                row.channel,
                row.review_id,
                row.candidate_id,
                row.revision_id,
                row.work_item_id,
                row.brief_slug,
                row.criterion_key,
                row.criterion_text,
                row.outcome,
                row.note,
                evidence,
                row.provenance,
                row.session_id,
                row.model,
                row.created_ms,
            ],
        )?;
        Ok(())
    }

    /// Record the revision a review session was started for: the candidate
    /// it exercises, whatever the review is resubmitted to later; and how it
    /// came to start, `submission` or `operator`.
    pub fn review_session_start_record(
        &self,
        session_id: &str,
        review_id: &str,
        revision_id: &str,
        started_by: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO review_session_revision
                (session_id, review_id, revision_id, created_ms, started_by)
             VALUES (?1,?2,?3,?4,?5)",
            params![session_id, review_id, revision_id, now_ms(), started_by],
        )?;
        Ok(())
    }

    /// Record the reviewer trigger that applied when a revision was submitted.
    pub fn review_revision_trigger_record(
        &self,
        review_id: &str,
        revision_id: &str,
        trigger: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO review_revision_trigger (revision_id, review_id, trigger)
             VALUES (?1,?2,?3)",
            params![revision_id, review_id, trigger],
        )?;
        Ok(())
    }

    /// The reviewer trigger that applied when a revision was submitted, when
    /// that was recorded.
    pub fn review_revision_trigger(&self, revision_id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT trigger FROM review_revision_trigger WHERE revision_id=?1",
                [revision_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// How a review session came to start, when that was recorded.
    pub fn review_session_started_by(&self, session_id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT started_by FROM review_session_revision WHERE session_id=?1",
                [session_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    /// The id of the revision a review session exercises, as
    /// [`Store::review_session_revision`] finds it, without reading the
    /// revision itself when it was recorded.
    pub fn review_session_revision_id(&self, session: &SessionRow) -> Result<Option<String>> {
        let recorded: Option<String> = {
            let conn = self.conn.lock().unwrap();
            conn.query_row(
                "SELECT revision_id FROM review_session_revision
                 WHERE session_id=?1 AND review_id=?2",
                params![session.id, session.review_id],
                |row| row.get(0),
            )
            .optional()?
        };
        match recorded {
            Some(id) => Ok(Some(id)),
            None => Ok(self.review_session_revision(session)?.map(|r| r.id)),
        }
    }

    /// The revision a review session exercises: the one recorded when it was
    /// started, or, for a session started before that was recorded, the
    /// review's latest revision created at or before the session.
    pub fn review_session_revision(
        &self,
        session: &SessionRow,
    ) -> Result<Option<ReviewRevisionRow>> {
        let Some(review_id) = session.review_id.as_deref() else {
            return Ok(None);
        };
        let recorded: Option<String> = {
            let conn = self.conn.lock().unwrap();
            conn.query_row(
                "SELECT revision_id FROM review_session_revision
                 WHERE session_id=?1 AND review_id=?2",
                [&session.id, review_id],
                |row| row.get(0),
            )
            .optional()?
        };
        if let Some(id) = recorded {
            if let Some(revision) = self.review_revision(&id)? {
                return Ok(Some(revision));
            }
        }
        Ok(self
            .review_revisions(review_id)?
            .into_iter()
            .rev()
            .find(|revision| revision.created_ms <= session.created_ms))
    }

    /// Every finding about one candidate of an item, newest first: the first
    /// one per criterion is the one that stands.
    pub fn review_findings_for_candidate(
        &self,
        work_item_id: &str,
        candidate_id: &str,
    ) -> Result<Vec<ReviewFindingRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM review_finding WHERE work_item_id=?1 AND candidate_id=?2
             ORDER BY created_ms DESC, rowid DESC",
        )?;
        let rows = stmt
            .query_map([work_item_id, candidate_id], ReviewFindingRow::from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}
