//! Review-session bookkeeping: the revision each review session was started
//! for and how it came to start, the reviewer trigger that applied to each
//! revision, and the repository a workspace no session imported answers to.

use rusqlite::{params, OptionalExtension};

use super::{now_ms, Result, ReviewRevisionRow, SessionRow, Store};

impl Store {
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

    /// Record the repository a workspace no session imported answers to,
    /// before a session is started on it (`environment::origin_repo`).
    pub fn record_workspace_origin(&self, workspace_id: &str, repo: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO workspace_origin (workspace_id, repo, created_ms)
             VALUES (?1,?2,?3)",
            params![workspace_id, repo, now_ms()],
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

    /// What [`Store::record_workspace_origin`] recorded for a workspace:
    /// `None` when nothing was, `Some(None)` for a workspace tied to no
    /// repository.
    pub fn workspace_origin(&self, workspace_id: &str) -> Result<Option<Option<String>>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT repo FROM workspace_origin WHERE workspace_id=?1",
                [workspace_id],
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

    /// Every review session started on a review, oldest first, with the
    /// revision each was started for: `(session_id, revision_id)`.
    pub fn review_sessions_of(&self, review_id: &str) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT session_id, revision_id FROM review_session_revision
             WHERE review_id=?1 ORDER BY created_ms ASC, session_id ASC",
        )?;
        let rows = stmt
            .query_map([review_id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
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
}
