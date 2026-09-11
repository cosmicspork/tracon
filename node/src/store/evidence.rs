//! Immutable candidate evidence. Candidate identities begin with a Git commit
//! identity; review prose, worktree paths, and mutable execution tags never
//! participate in a reuse key.

use std::collections::BTreeSet;

use rusqlite::{params, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{now_ms, Result, ReviewRow, Store, StoreError};

/// A source snapshot that checks and later demonstrations may name. The id is
/// deliberately derived from commit and channel, never from a worktree path.
pub fn candidate_id(head_sha: &str, channel: &str) -> String {
    format!("{head_sha}:{channel}")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateRow {
    pub id: String,
    pub head_sha: String,
    pub tree_sha: Option<String>,
    pub channel: String,
    pub owner_session_id: String,
    pub source_kind: String,
    pub captured_ms: i64,
    /// How the source was captured. It may state that old provenance was not
    /// recorded; it must never fill missing image or input facts with guesses.
    pub capture_json: String,
}

impl CandidateRow {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            head_sha: row.get("head_sha")?,
            tree_sha: row.get("tree_sha")?,
            channel: row.get("channel")?,
            owner_session_id: row.get("owner_session_id")?,
            source_kind: row.get("source_kind")?,
            captured_ms: row.get("captured_ms")?,
            capture_json: row.get("capture_json")?,
        })
    }
}

/// One exact tree entry captured from Git. `mode` preserves regular,
/// executable, and safe-relative symbolic-link entries without carrying a
/// `.git` directory or a worktree pathname.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateFile {
    pub path: String,
    pub mode: u32,
    pub content: Vec<u8>,
}

/// An invocation of one trusted required check. `outcome` is one of `running`,
/// `passed`, `failed`, `interrupted`, `cancelled`, or `reused`; the source
/// outcome remains in `source_outcome` for a reused record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckRunRow {
    pub id: String,
    pub candidate_id: Option<String>,
    pub session_id: String,
    pub definition_json: String,
    pub definition_hash: Option<String>,
    pub execution_image: Option<String>,
    pub inputs_json: Option<String>,
    pub reuse_key: Option<String>,
    pub outcome: String,
    pub source_outcome: Option<String>,
    pub exit_code: Option<i64>,
    pub log: String,
    pub duration_ms: Option<i64>,
    pub started_ms: i64,
    pub finished_ms: Option<i64>,
    pub rerun_of: Option<String>,
    pub reused_from_id: Option<String>,
    pub metadata_json: String,
}

impl CheckRunRow {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            candidate_id: row.get("candidate_id")?,
            session_id: row.get("session_id")?,
            definition_json: row.get("definition_json")?,
            definition_hash: row.get("definition_hash")?,
            execution_image: row.get("execution_image")?,
            inputs_json: row.get("inputs_json")?,
            reuse_key: row.get("reuse_key")?,
            outcome: row.get("outcome")?,
            source_outcome: row.get("source_outcome")?,
            exit_code: row.get("exit_code")?,
            log: row.get("log")?,
            duration_ms: row.get("duration_ms")?,
            started_ms: row.get("started_ms")?,
            finished_ms: row.get("finished_ms")?,
            rerun_of: row.get("rerun_of")?,
            reused_from_id: row.get("reused_from_id")?,
            metadata_json: row.get("metadata_json")?,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewRevisionRow {
    pub id: String,
    pub review_id: String,
    pub candidate_id: String,
    pub title: String,
    pub body: String,
    pub diff: String,
    pub files: String,
    pub head_sha: String,
    /// Pinned excerpts adjacent to changed lines at submit time.
    pub context_json: String,
    pub created_ms: i64,
}

impl ReviewRevisionRow {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            review_id: row.get("review_id")?,
            candidate_id: row.get("candidate_id")?,
            title: row.get("title")?,
            body: row.get("body")?,
            diff: row.get("diff")?,
            files: row.get("files")?,
            head_sha: row.get("head_sha")?,
            context_json: row.get("context_json")?,
            created_ms: row.get("created_ms")?,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewDecisionRow {
    pub id: String,
    pub review_id: String,
    pub revision_id: String,
    /// `operator`, `authority`, or `legacy_unknown`; never inferred from state.
    pub source: String,
    pub decision: String,
    pub reason: Option<String>,
    pub title: Option<String>,
    pub body: Option<String>,
    pub patch: Option<String>,
    pub decided_ms: i64,
}

impl ReviewDecisionRow {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            review_id: row.get("review_id")?,
            revision_id: row.get("revision_id")?,
            source: row.get("source")?,
            decision: row.get("decision")?,
            reason: row.get("reason")?,
            title: row.get("title")?,
            body: row.get("body")?,
            patch: row.get("patch")?,
            decided_ms: row.get("decided_ms")?,
        })
    }
}

/// Curated material deliberately linked to a candidate. It is a document
/// snapshot, not an execution record, so a Showboat document stays evidence a
/// human chose to attach rather than a command the review UI will run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DemonstrationRow {
    pub id: String,
    pub candidate_id: String,
    pub channel: String,
    pub document_id: String,
    pub document_slug: String,
    pub document_hash: String,
    pub label: String,
    pub created_ms: i64,
}

impl DemonstrationRow {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            candidate_id: row.get("candidate_id")?,
            channel: row.get("channel")?,
            document_id: row.get("document_id")?,
            document_slug: row.get("document_slug")?,
            document_hash: row.get("document_hash")?,
            label: row.get("label")?,
            created_ms: row.get("created_ms")?,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CandidateEvidence {
    pub candidate: CandidateRow,
    pub checks: Vec<CheckRunRow>,
    pub revisions: Vec<ReviewRevisionRow>,
    pub decisions: Vec<ReviewDecisionRow>,
    pub demonstrations: Vec<DemonstrationRow>,
}

impl Store {
    pub fn insert_candidate(&self, candidate: &CandidateRow) -> Result<bool> {
        let conn = self.conn.lock().map_err(|_| StoreError::Invalid("store lock poisoned".into()))?;
        Ok(conn.execute(
            "INSERT OR IGNORE INTO candidate
                (id, head_sha, tree_sha, channel, owner_session_id, source_kind, captured_ms, capture_json)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                candidate.id,
                candidate.head_sha,
                candidate.tree_sha,
                candidate.channel,
                candidate.owner_session_id,
                candidate.source_kind,
                candidate.captured_ms,
                candidate.capture_json,
            ],
        )? == 1)
    }

    pub fn insert_candidate_files(&self, candidate_id: &str, files: &[CandidateFile]) -> Result<()> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| StoreError::Invalid("store lock poisoned".into()))?;
        let tx = conn.transaction()?;
        for file in files {
            tx.execute(
                "INSERT OR IGNORE INTO candidate_file (candidate_id, path, mode, content, size_bytes)
                 VALUES (?1,?2,?3,?4,?5)",
                params![
                    candidate_id,
                    file.path,
                    i64::from(file.mode),
                    file.content,
                    i64::try_from(file.content.len())
                        .map_err(|_| StoreError::Invalid("candidate file is too large".into()))?,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn candidate_files(&self, candidate_id: &str) -> Result<Vec<CandidateFile>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| StoreError::Invalid("store lock poisoned".into()))?;
        let mut stmt = conn.prepare(
            "SELECT path, mode, content FROM candidate_file WHERE candidate_id=?1 ORDER BY path",
        )?;
        let rows = stmt.query_map([candidate_id], |row| {
            Ok(CandidateFile {
                path: row.get(0)?,
                mode: u32::try_from(row.get::<_, i64>(1)?).unwrap_or_default(),
                content: row.get(2)?,
            })
        })?
        .collect::<std::result::Result<_, _>>();
        rows.map_err(Into::into)
    }

    pub fn materialize_candidate(
        &self,
        candidate_id: &str,
        destination: &std::path::Path,
    ) -> Result<()> {
        let files = self.candidate_files(candidate_id)?;
        crate::review::materialize_candidate_files(destination, &files)
            .map_err(|error| StoreError::Invalid(error.to_string()))
    }

    /// Fill a legacy candidate's absent materialization facts from a new,
    /// hardened capture. Existing immutable capture data is never overwritten.
    pub fn record_candidate_snapshot(
        &self,
        candidate_id: &str,
        tree_sha: &str,
        capture_json: &str,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| StoreError::Invalid("store lock poisoned".into()))?;
        conn.execute(
            "UPDATE candidate
             SET tree_sha=COALESCE(tree_sha, ?2), source_kind=CASE WHEN tree_sha IS NULL THEN 'git' ELSE source_kind END,
                 capture_json=CASE WHEN tree_sha IS NULL THEN ?3 ELSE capture_json END
             WHERE id=?1",
            params![candidate_id, tree_sha, capture_json],
        )?;
        Ok(())
    }

    pub fn candidate(&self, id: &str) -> Result<Option<CandidateRow>> {
        let conn = self.conn.lock().map_err(|_| StoreError::Invalid("store lock poisoned".into()))?;
        conn.query_row("SELECT * FROM candidate WHERE id=?1", [id], CandidateRow::from_row)
            .optional()
            .map_err(Into::into)
    }

    pub fn candidate_by_commit(&self, channel: &str, head_sha: &str) -> Result<Option<CandidateRow>> {
        let conn = self.conn.lock().map_err(|_| StoreError::Invalid("store lock poisoned".into()))?;
        conn.query_row(
            "SELECT * FROM candidate WHERE channel=?1 AND head_sha=?2 ORDER BY captured_ms DESC LIMIT 1",
            params![channel, head_sha],
            CandidateRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn insert_check_run(&self, run: &CheckRunRow) -> Result<()> {
        let conn = self.conn.lock().map_err(|_| StoreError::Invalid("store lock poisoned".into()))?;
        conn.execute(
            "INSERT INTO check_run (id, candidate_id, session_id, definition_json, definition_hash,
                execution_image, inputs_json, reuse_key, outcome, source_outcome, exit_code, log,
                duration_ms, started_ms, finished_ms, rerun_of, reused_from_id, metadata_json)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
            params![
                run.id, run.candidate_id, run.session_id, run.definition_json, run.definition_hash,
                run.execution_image, run.inputs_json, run.reuse_key, run.outcome, run.source_outcome,
                run.exit_code, run.log, run.duration_ms, run.started_ms, run.finished_ms, run.rerun_of,
                run.reused_from_id, run.metadata_json,
            ],
        )?;
        Ok(())
    }

    pub fn finish_check_run(
        &self,
        id: &str,
        outcome: &str,
        source_outcome: Option<&str>,
        exit_code: Option<i64>,
        log: &str,
        duration_ms: Option<i64>,
        metadata: &Value,
    ) -> Result<bool> {
        let conn = self.conn.lock().map_err(|_| StoreError::Invalid("store lock poisoned".into()))?;
        let changed = conn.execute(
            "UPDATE check_run SET outcome=?2, source_outcome=?3, exit_code=?4, log=?5,
                duration_ms=?6, finished_ms=?7, metadata_json=?8
             WHERE id=?1 AND outcome='running'",
            params![id, outcome, source_outcome, exit_code, log, duration_ms, now_ms(), metadata.to_string()],
        )?;
        Ok(changed == 1)
    }

    /// Record a controller-confirmed cancellation. Callers must stop the
    /// runner first; this method deliberately does not claim to cancel work it
    /// cannot reach.
    pub fn cancel_running_check(&self, id: &str, log: &str, metadata: &Value) -> Result<bool> {
        self.finish_check_run(id, "cancelled", None, None, log, None, metadata)
    }

    /// Only terminal, exactly identified runs can be reused. A reuse key is
    /// absent whenever the executing image was not immutable.
    pub fn latest_reusable_check(&self, candidate_id: &str, reuse_key: &str) -> Result<Option<CheckRunRow>> {
        let conn = self.conn.lock().map_err(|_| StoreError::Invalid("store lock poisoned".into()))?;
        conn.query_row(
            "SELECT * FROM check_run WHERE candidate_id=?1 AND reuse_key=?2
                AND outcome IN ('passed','failed','reused')
             ORDER BY finished_ms DESC LIMIT 1",
            params![candidate_id, reuse_key],
            CheckRunRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// A direct run against the exact current definition and execution
    /// configuration. This is used when an image is mutable: it may support
    /// the submission that just ran, but is never selected as reusable proof.
    pub fn latest_matching_check(
        &self,
        candidate_id: &str,
        definition_hash: &str,
        execution_image: Option<&str>,
        inputs_json: &str,
    ) -> Result<Option<CheckRunRow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| StoreError::Invalid("store lock poisoned".into()))?;
        conn.query_row(
            "SELECT * FROM check_run
             WHERE candidate_id=?1 AND definition_hash=?2 AND execution_image IS ?3
               AND inputs_json=?4 AND outcome='passed'
             ORDER BY finished_ms DESC LIMIT 1",
            params![candidate_id, definition_hash, execution_image, inputs_json],
            CheckRunRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// The most recent terminal invocation with these exact recorded inputs,
    /// including a failed one. An explicit rerun links to it but never adopts
    /// its outcome.
    pub fn latest_check_for_identity(
        &self,
        candidate_id: &str,
        definition_hash: &str,
        execution_image: Option<&str>,
        inputs_json: &str,
    ) -> Result<Option<CheckRunRow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| StoreError::Invalid("store lock poisoned".into()))?;
        conn.query_row(
            "SELECT * FROM check_run
             WHERE candidate_id=?1 AND definition_hash=?2 AND execution_image IS ?3
               AND inputs_json=?4 AND outcome <> 'running'
             ORDER BY finished_ms DESC LIMIT 1",
            params![candidate_id, definition_hash, execution_image, inputs_json],
            CheckRunRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn check_runs_for_candidate(&self, candidate_id: &str) -> Result<Vec<CheckRunRow>> {
        let conn = self.conn.lock().map_err(|_| StoreError::Invalid("store lock poisoned".into()))?;
        let mut stmt = conn.prepare(
            "SELECT * FROM check_run WHERE candidate_id=?1 ORDER BY started_ms ASC, id ASC",
        )?;
        let rows = stmt.query_map([candidate_id], CheckRunRow::from_row)?
            .collect::<std::result::Result<_, _>>();
        rows.map_err(Into::into)
    }


    /// Make an operator-visible review and its immutable first revision one
    /// transaction. A review that lacks a revision is never emitted by a new
    /// submission path.
    pub fn insert_review_with_revision(
        &self,
        review: &ReviewRow,
        revision: &ReviewRevisionRow,
    ) -> Result<()> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| StoreError::Invalid("store lock poisoned".into()))?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO review (id, session_id, node_id, channel, kind, title, body, edited_title,
                edited_body, provider, target, diff, files, head_sha, base_ref, added, removed,
                state, verdict_reason, publish_result, claimed_ms, created_ms, created_mono_ms,
                resolved_mono_ms, updated_ms, checks_json, review_session_id, ai_verdict_json, revision_patch)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,
                ?22,?23,?24,?25,?26,?27,?28,?29)",
            params![
                review.id, review.session_id, review.node_id, review.channel, review.kind,
                review.title, review.body, review.edited_title, review.edited_body, review.provider,
                review.target, review.diff, review.files, review.head_sha, review.base_ref, review.added,
                review.removed, review.state, review.verdict_reason, review.publish_result, review.claimed_ms,
                review.created_ms, review.created_mono_ms, review.resolved_mono_ms, review.updated_ms,
                review.checks_json, review.review_session_id, review.ai_verdict_json, review.revision_patch,
            ],
        )?;
        Self::write_review_revision(&tx, revision)?;
        tx.commit()?;
        Ok(())
    }

    /// Replace the mutable card projection and append its immutable revision
    /// atomically after a candidate has passed its checks. Only a review
    /// currently awaiting a revision accepts one: the same race-safe gate
    /// `revise_review` enforced, now preserved alongside the revision row.
    pub fn revise_review_with_revision(
        &self,
        review_id: &str,
        title: &str,
        body: &str,
        added: i64,
        removed: i64,
        checks_json: Option<&str>,
        revision: &ReviewRevisionRow,
    ) -> Result<bool> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| StoreError::Invalid("store lock poisoned".into()))?;
        let tx = conn.transaction()?;
        let changed = tx.execute(
            "UPDATE review SET title=?2, body=?3, diff=?4, files=?5, head_sha=?6, added=?7,
                removed=?8, checks_json=?9, state='new', verdict_reason=NULL, revision_patch=NULL,
                claimed_ms=NULL, resolved_mono_ms=NULL, updated_ms=?10 WHERE id=?1 AND state='revising'",
            params![
                review_id, title, body, revision.diff, revision.files, revision.head_sha, added, removed,
                checks_json, now_ms(),
            ],
        )?;
        if changed != 1 {
            tx.rollback()?;
            return Ok(false);
        }
        Self::write_review_revision(&tx, revision)?;
        tx.commit()?;
        Ok(true)
    }

    fn write_review_revision(
        tx: &rusqlite::Transaction<'_>,
        revision: &ReviewRevisionRow,
    ) -> rusqlite::Result<()> {
        tx.execute(
            "INSERT INTO review_revision
                (id, review_id, candidate_id, title, body, diff, files, head_sha, context_json, created_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                revision.id, revision.review_id, revision.candidate_id, revision.title, revision.body,
                revision.diff, revision.files, revision.head_sha, revision.context_json, revision.created_ms,
            ],
        )?;
        Ok(())
    }

    pub fn review_revisions(&self, review_id: &str) -> Result<Vec<ReviewRevisionRow>> {
        let conn = self.conn.lock().map_err(|_| StoreError::Invalid("store lock poisoned".into()))?;
        let mut stmt = conn.prepare(
            "SELECT * FROM review_revision WHERE review_id=?1 ORDER BY created_ms ASC, id ASC",
        )?;
        let rows = stmt.query_map([review_id], ReviewRevisionRow::from_row)?
            .collect::<std::result::Result<_, _>>();
        rows.map_err(Into::into)
    }

    pub fn latest_review_revision(&self, review_id: &str) -> Result<Option<ReviewRevisionRow>> {
        let conn = self.conn.lock().map_err(|_| StoreError::Invalid("store lock poisoned".into()))?;
        conn.query_row(
            "SELECT * FROM review_revision WHERE review_id=?1 ORDER BY created_ms DESC, id DESC LIMIT 1",
            [review_id],
            ReviewRevisionRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn record_review_decision(&self, decision: &ReviewDecisionRow) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| StoreError::Invalid("store lock poisoned".into()))?;
        conn.execute(
            "INSERT INTO review_decision
                (id, review_id, revision_id, decision, source, reason, title, body, patch, decided_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                decision.id, decision.review_id, decision.revision_id, decision.decision, decision.source,
                decision.reason, decision.title, decision.body, decision.patch, decision.decided_ms,
            ],
        )?;
        Ok(())
    }

    pub fn review_decisions(&self, review_id: &str) -> Result<Vec<ReviewDecisionRow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| StoreError::Invalid("store lock poisoned".into()))?;
        let mut stmt = conn.prepare(
            "SELECT * FROM review_decision WHERE review_id=?1 ORDER BY decided_ms ASC, id ASC",
        )?;
        let rows = stmt.query_map([review_id], ReviewDecisionRow::from_row)?
            .collect::<std::result::Result<_, _>>();
        rows.map_err(Into::into)
    }
    pub fn attach_demonstration(&self, demonstration: &DemonstrationRow) -> Result<()> {
        let conn = self.conn.lock().map_err(|_| StoreError::Invalid("store lock poisoned".into()))?;
        conn.execute(
            "INSERT INTO demonstration
                (id, candidate_id, channel, document_id, document_slug, document_hash, label, created_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                demonstration.id, demonstration.candidate_id, demonstration.channel,
                demonstration.document_id, demonstration.document_slug, demonstration.document_hash,
                demonstration.label, demonstration.created_ms,
            ],
        )?;
        Ok(())
    }

    pub fn demonstrations_for_candidate(&self, candidate_id: &str) -> Result<Vec<DemonstrationRow>> {
        let conn = self.conn.lock().map_err(|_| StoreError::Invalid("store lock poisoned".into()))?;
        let mut stmt = conn.prepare(
            "SELECT * FROM demonstration WHERE candidate_id=?1 ORDER BY created_ms ASC, id ASC",
        )?;
        let rows = stmt.query_map([candidate_id], DemonstrationRow::from_row)?
            .collect::<std::result::Result<_, _>>();
        rows.map_err(Into::into)
    }

    pub fn candidate_evidence(&self, candidate_id: &str) -> Result<CandidateEvidence> {
        let candidate = self
            .candidate(candidate_id)?
            .ok_or_else(|| StoreError::Invalid("no such candidate".into()))?;
        let revisions = {
            let conn = self.conn.lock().map_err(|_| StoreError::Invalid("store lock poisoned".into()))?;
            let mut stmt = conn.prepare(
                "SELECT * FROM review_revision WHERE candidate_id=?1 ORDER BY created_ms ASC, id ASC",
            )?;
            let rows = stmt.query_map([candidate_id], ReviewRevisionRow::from_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows
        };
        let review_ids: BTreeSet<&str> = revisions
            .iter()
            .map(|revision| revision.review_id.as_str())
            .collect();
        let mut decisions = Vec::new();
        for review_id in review_ids {
            decisions.extend(self.review_decisions(review_id)?);
        }
        decisions.sort_by_key(|decision| decision.decided_ms);
        Ok(CandidateEvidence {
            candidate,
            checks: self.check_runs_for_candidate(candidate_id)?,
            revisions,
            decisions,
            demonstrations: self.demonstrations_for_candidate(candidate_id)?,
        })
    }

    /// Legacy check events have no immutable candidate identity. Keep them
    /// discoverable by their session rather than pretending an association.
    pub fn legacy_check_runs_for_session(&self, session_id: &str) -> Result<Vec<CheckRunRow>> {
        let conn = self.conn.lock().map_err(|_| StoreError::Invalid("store lock poisoned".into()))?;
        let mut stmt = conn.prepare(
            "SELECT * FROM check_run WHERE session_id=?1 AND candidate_id IS NULL ORDER BY started_ms ASC, id ASC",
        )?;
        let rows = stmt.query_map([session_id], CheckRunRow::from_row)?
            .collect::<std::result::Result<_, _>>();
        rows.map_err(Into::into)
    }
}
