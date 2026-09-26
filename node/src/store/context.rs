//! Context receipts: what each attempt at a work item received of the
//! documents the operator selected for it (`corpus::context`).
//!
//! One row per session, written at launch and never rewritten. The revision
//! is counted per item over distinct digests, in the same lock as the insert,
//! so two attempts that received the same bytes share a revision and the
//! first attempt to receive something new mints the next one.

use rusqlite::{params, OptionalExtension};

use super::{Result, Store};
use crate::corpus::context::Receipt;

impl Store {
    /// Record a receipt, filling in its revision. Returns the receipt as it
    /// is now identified.
    pub fn context_receipt_record(&self, receipt: &Receipt) -> Result<Receipt> {
        let conn = self.conn.lock().unwrap();
        let existing: Option<i64> = conn
            .query_row(
                "SELECT revision FROM context_receipt WHERE work_item_id=?1 AND digest=?2
                 LIMIT 1",
                params![receipt.work_item_id, receipt.digest],
                |r| r.get(0),
            )
            .optional()?;
        let revision = match existing {
            Some(revision) => revision,
            None => conn.query_row(
                "SELECT COALESCE(MAX(revision), 0) + 1 FROM context_receipt WHERE work_item_id=?1",
                [&receipt.work_item_id],
                |r| r.get(0),
            )?,
        };
        let mut recorded = receipt.clone();
        recorded.revision = revision;
        conn.execute(
            "INSERT INTO context_receipt
                (session_id, work_item_id, revision, digest, body, created_ms)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                recorded.session_id,
                recorded.work_item_id,
                revision,
                recorded.digest,
                serde_json::to_string(&recorded)?,
                recorded.created_ms,
            ],
        )?;
        Ok(recorded)
    }

    /// Every attempt's receipt for an item, oldest first.
    pub fn context_receipts(&self, work_item_id: &str) -> Result<Vec<Receipt>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT body FROM context_receipt WHERE work_item_id=?1
             ORDER BY created_ms, rowid",
        )?;
        let rows = stmt
            .query_map([work_item_id], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows
            .iter()
            .filter_map(|b| serde_json::from_str(b).ok())
            .collect())
    }

    /// The most recent attempt's receipt for an item.
    pub fn context_receipt_latest(&self, work_item_id: &str) -> Result<Option<Receipt>> {
        let conn = self.conn.lock().unwrap();
        let body: Option<String> = conn
            .query_row(
                "SELECT body FROM context_receipt WHERE work_item_id=?1
                 ORDER BY created_ms DESC, rowid DESC LIMIT 1",
                [work_item_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(body.and_then(|b| serde_json::from_str(&b).ok()))
    }

    /// What one session received.
    pub fn context_receipt_for(&self, session_id: &str) -> Result<Option<Receipt>> {
        let conn = self.conn.lock().unwrap();
        let body: Option<String> = conn
            .query_row(
                "SELECT body FROM context_receipt WHERE session_id=?1",
                [session_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(body.and_then(|b| serde_json::from_str(&b).ok()))
    }
}
