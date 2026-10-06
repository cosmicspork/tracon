//! Work an agent shows the operator (`show_work`).
//!
//! The row is the agent's account — its Markdown, and the HTML bundle document
//! holding its page or files — beside the two things the node knows itself: the
//! commit the work was shown at, and the hash of the bundle it stored. Staleness
//! is computed on read against the review it is shown in, never stored: it is a
//! fact about the present, not part of what was shown.

use rusqlite::{params, Row};
use serde::{Deserialize, Serialize};

use super::{candidate_id, Result, ReviewRow, Store, StoreError};

pub const MARKDOWN: &str = "markdown";
pub const HTML: &str = "html";
pub const FILES: &str = "files";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShownFile {
    pub path: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShownWorkRow {
    pub id: String,
    pub channel: String,
    pub session_id: Option<String>,
    /// What an external harness labels itself, for display. Never authority.
    pub lane: Option<String>,
    /// The review it was shown on, when the agent named one. Without it, the
    /// work belongs to its session and appears on that session's reviews.
    pub review_id: Option<String>,
    /// The commit the workspace was at when the work was shown.
    pub head_sha: String,
    pub title: String,
    /// `markdown` (an account and nothing else), `html` (the agent's page and
    /// what it loads) or `files` (plain files, under a listing the node wrote).
    pub format: String,
    pub markdown: String,
    pub document_id: Option<String>,
    pub document_slug: Option<String>,
    pub document_hash: Option<String>,
    pub files: Vec<ShownFile>,
    pub created_ms: i64,
}

impl ShownWorkRow {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        let files: String = row.get("files_json")?;
        Ok(Self {
            id: row.get("id")?,
            channel: row.get("channel")?,
            session_id: row.get("session_id")?,
            lane: row.get("lane")?,
            review_id: row.get("review_id")?,
            head_sha: row.get("head_sha")?,
            title: row.get("title")?,
            format: row.get("format")?,
            markdown: row.get("markdown")?,
            document_id: row.get("document_id")?,
            document_slug: row.get("document_slug")?,
            document_hash: row.get("document_hash")?,
            files: serde_json::from_str(&files).unwrap_or_default(),
            created_ms: row.get("created_ms")?,
        })
    }
}

/// A required check the node ran on the commit the work was shown at.
#[derive(Debug, Clone, Serialize)]
pub struct ShownCheck {
    pub command: Option<String>,
    pub outcome: String,
    /// What a reused run's source concluded.
    pub source_outcome: Option<String>,
}

/// Shown work as the operator reads it: the agent's account, with what the
/// node vouches for beside it.
#[derive(Debug, Clone, Serialize)]
pub struct ShownWorkView {
    #[serde(flatten)]
    pub row: ShownWorkRow,
    /// True when the work no longer describes what is under review: the
    /// candidate moved past the commit it was shown at, or its stored bundle
    /// is gone or was replaced.
    pub stale: bool,
    pub stale_reason: Option<String>,
    /// The required checks the node ran on `head_sha`, whatever the agent says.
    pub checks: Vec<ShownCheck>,
}

impl Store {
    pub fn insert_shown_work(&self, row: &ShownWorkRow) -> Result<()> {
        let files = serde_json::to_string(&row.files)
            .map_err(|error| StoreError::Invalid(error.to_string()))?;
        let conn = self
            .conn
            .lock()
            .map_err(|_| StoreError::Invalid("store lock poisoned".into()))?;
        conn.execute(
            "INSERT INTO shown_work
                (id, channel, session_id, lane, review_id, head_sha, title, format, markdown,
                 document_id, document_slug, document_hash, files_json, created_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![
                row.id,
                row.channel,
                row.session_id,
                row.lane,
                row.review_id,
                row.head_sha,
                row.title,
                row.format,
                row.markdown,
                row.document_id,
                row.document_slug,
                row.document_hash,
                files,
                row.created_ms,
            ],
        )?;
        Ok(())
    }

    fn shown_work_where(
        &self,
        clause: &str,
        args: &[&dyn rusqlite::ToSql],
    ) -> Result<Vec<ShownWorkRow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| StoreError::Invalid("store lock poisoned".into()))?;
        let mut stmt = conn.prepare(&format!(
            "SELECT * FROM shown_work WHERE {clause} ORDER BY created_ms ASC, id ASC"
        ))?;
        let rows = stmt
            .query_map(args, ShownWorkRow::from_row)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// Everything a session showed, oldest first, read against nothing: a
    /// session can have several reviews, and each judges staleness itself.
    pub fn shown_work_for_session(&self, session_id: &str) -> Result<Vec<ShownWorkView>> {
        self.shown_work_where("session_id=?1", &[&session_id])?
            .into_iter()
            .map(|row| self.shown_work_view(row, None))
            .collect()
    }

    /// The work shown on a review: what named it, and what its session showed
    /// without naming one. Each is stale once the review's candidate is no
    /// longer the commit it was shown at.
    pub fn shown_work_for_review(&self, review: &ReviewRow) -> Result<Vec<ShownWorkView>> {
        let rows = match review.session_id.as_deref() {
            Some(session_id) => self.shown_work_where(
                "review_id=?1 OR (review_id IS NULL AND session_id=?2)",
                &[&review.id, &session_id],
            )?,
            None => self.shown_work_where("review_id=?1", &[&review.id])?,
        };
        let head = self
            .latest_review_revision(&review.id)?
            .map(|revision| revision.head_sha)
            .unwrap_or_else(|| review.head_sha.clone());
        rows.into_iter()
            .map(|row| self.shown_work_view(row, Some(&head)))
            .collect()
    }

    fn shown_work_view(&self, row: ShownWorkRow, head: Option<&str>) -> Result<ShownWorkView> {
        let bundle_changed = match (&row.document_id, &row.document_hash) {
            (Some(id), Some(hash)) => match self.doc_by_id(id)? {
                Some(doc) => doc.deleted != 0 || &doc.hash != hash,
                None => true,
            },
            _ => false,
        };
        let stale_reason = if bundle_changed {
            Some("its stored files were removed or replaced since it was shown".to_string())
        } else {
            head.filter(|head| *head != row.head_sha).map(|head| {
                format!(
                    "shown at {}; the candidate is now {}",
                    short(&row.head_sha),
                    short(head)
                )
            })
        };
        let checks = self
            .check_runs_for_candidate(&candidate_id(&row.head_sha, &row.channel))?
            .into_iter()
            .map(|run| ShownCheck {
                command: run.command,
                outcome: run.outcome,
                source_outcome: run.source_outcome,
            })
            .collect();
        Ok(ShownWorkView {
            stale: stale_reason.is_some(),
            stale_reason,
            checks,
            row,
        })
    }
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}
