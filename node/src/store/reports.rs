//! State transitions for standalone narrative reports.
//!
//! Reports reuse the replicated review row so they arrive in the same durable
//! operator queue, but their `kind` makes their limited transition set explicit:
//! acknowledgment records receipt only and can never enter publication.

use super::{now_ms, Result, Store};

pub const KIND: &str = "report";
pub const ACKNOWLEDGED: &str = "acknowledged";

impl Store {
    /// Replace a narrative report after the operator requested changes. The
    /// content hash is the version precondition a later acknowledgement names;
    /// it is not a Git revision.
    pub fn resubmit_report(
        &self,
        id: &str,
        title: &str,
        body: &str,
        content_hash: &str,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE review
             SET title=?2, body=?3, head_sha=?4, state='new', verdict_reason=NULL,
                 claimed_ms=NULL, resolved_mono_ms=NULL, updated_ms=?5
             WHERE id=?1 AND kind='report' AND state IN ('new','claimed','revising')",
            rusqlite::params![id, title, body, content_hash, now_ms()],
        )?;
        Ok(changed == 1)
    }

    /// Record an operator acknowledgement without invoking any publication
    /// machinery. A report has no code candidate and no forge target.
    pub fn acknowledge_report(
        &self,
        id: &str,
        note: Option<&str>,
        content_hash: &str,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE review
             SET state='acknowledged', verdict_reason=?2, resolved_mono_ms=?3, updated_ms=?3
             WHERE id=?1 AND kind='report' AND state IN ('new','claimed') AND head_sha=?4",
            rusqlite::params![id, note, now_ms(), content_hash],
        )?;
        Ok(changed == 1)
    }

    /// Keep the report visible while its submitter revises it. This transition
    /// is deliberately distinct from a rejection: report feedback is returned
    /// through `report_status`, not a code-review or publication path.
    pub fn request_report_changes(&self, id: &str, note: &str, content_hash: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE review
             SET state='revising', verdict_reason=?2, claimed_ms=NULL, updated_ms=?3
             WHERE id=?1 AND kind='report' AND state IN ('new','claimed') AND head_sha=?4",
            rusqlite::params![id, note, now_ms(), content_hash],
        )?;
        Ok(changed == 1)
    }
}
