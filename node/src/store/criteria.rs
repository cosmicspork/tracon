//! Human verdicts on individual acceptance criteria.
//!
//! One row is one person saying, of one criterion and one candidate, whether it
//! was met. Append-only: a later verdict supersedes an earlier one and both are
//! kept, because "the operator changed their mind" is itself worth reading.
//!
//! Node-local: this node's operator judging this node's candidate must not
//! become another node's observation. Nothing here
//! replicates, and no agent-reachable path writes it — the only writer is
//! `corpus::criteria::judge`, which refuses a session outright.
//!
//! The criterion's text is stored beside its key as it stood when judged. A
//! criterion is named from its own text, so rewording one mints a new name; the
//! old verdict is then reported as orphaned, with the wording it was about,
//! rather than silently following a line that now means something else.

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::{Result, Store};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CriterionJudgementRow {
    pub id: String,
    pub channel: String,
    pub work_item_id: String,
    pub brief_slug: String,
    /// The criterion's name, derived from its text by `corpus::criteria`.
    pub criterion_key: String,
    /// The criterion as it read when it was judged.
    pub criterion_text: String,
    /// The candidate this verdict is about. `None` means the operator judged
    /// the criterion itself rather than a particular attempt at it.
    pub candidate_id: Option<String>,
    pub revision_id: Option<String>,
    /// `met`, `not_met` or `unclear`.
    pub verdict: String,
    pub note: Option<String>,
    pub judged_ms: i64,
}

impl CriterionJudgementRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            channel: row.get("channel")?,
            work_item_id: row.get("work_item_id")?,
            brief_slug: row.get("brief_slug")?,
            criterion_key: row.get("criterion_key")?,
            criterion_text: row.get("criterion_text")?,
            candidate_id: row.get("candidate_id")?,
            revision_id: row.get("revision_id")?,
            verdict: row.get("verdict")?,
            note: row.get("note")?,
            judged_ms: row.get("judged_ms")?,
        })
    }
}

/// The three things a person can say about a criterion. `unclear` is not a
/// hedge: it records that the criterion as written could not be judged, which
/// is a finding about the criterion rather than about the work.
pub const VERDICTS: &[&str] = &["met", "not_met", "unclear"];

const SELECT: &str = "SELECT id, channel, work_item_id, brief_slug, criterion_key,
     criterion_text, candidate_id, revision_id, verdict, note, judged_ms
     FROM criterion_judgement";

impl Store {
    /// Record one verdict. The caller supplies the id and the timestamp so the
    /// row it gets back is the row that was written.
    pub fn criterion_judgement_record(&self, row: &CriterionJudgementRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO criterion_judgement
                (id, channel, work_item_id, brief_slug, criterion_key, criterion_text,
                 candidate_id, revision_id, verdict, note, judged_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                row.id,
                row.channel,
                row.work_item_id,
                row.brief_slug,
                row.criterion_key,
                row.criterion_text,
                row.candidate_id,
                row.revision_id,
                row.verdict,
                row.note,
                row.judged_ms,
            ],
        )?;
        Ok(())
    }

    /// Every verdict recorded against an item, newest first. The reader picks
    /// which ones are about the candidate it is looking at; the ones that are
    /// not are how "judged for an earlier candidate" is known.
    pub fn criterion_judgements_for_item(
        &self,
        work_item_id: &str,
    ) -> Result<Vec<CriterionJudgementRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "{SELECT} WHERE work_item_id=?1 ORDER BY judged_ms DESC, rowid DESC"
        ))?;
        let rows = stmt
            .query_map([work_item_id], CriterionJudgementRow::from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The standing verdict on one criterion of one candidate: the newest one,
    /// which is what supersedes the rest.
    pub fn criterion_judgement_latest(
        &self,
        work_item_id: &str,
        criterion_key: &str,
        candidate_id: Option<&str>,
    ) -> Result<Option<CriterionJudgementRow>> {
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "{SELECT} WHERE work_item_id=?1 AND criterion_key=?2
             AND candidate_id IS ?3
             ORDER BY judged_ms DESC, rowid DESC LIMIT 1"
        );
        let row = conn
            .query_row(
                &sql,
                params![work_item_id, criterion_key, candidate_id],
                CriterionJudgementRow::from_row,
            )
            .optional()?;
        Ok(row)
    }
}
