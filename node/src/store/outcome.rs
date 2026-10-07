//! The reads an outcome record is built from. Each is a plain read of rows
//! other paths wrote; nothing here writes.

use super::{EventRow, Result, ReviewRow, Store};

impl Store {
    /// Every review and report a session submitted, oldest first.
    pub fn reviews_of_session(&self, session_id: &str) -> Result<Vec<ReviewRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT * FROM review WHERE session_id=?1 ORDER BY created_ms ASC, id ASC")?;
        let rows = stmt
            .query_map([session_id], ReviewRow::from_row)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// A session's events of one kind, oldest first.
    pub fn session_events_of_kind(&self, session_id: &str, kind: &str) -> Result<Vec<EventRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT seq, node_id, session_id, work_item_id, kind, ref_id, payload, at_ms, mono_ms
             FROM event WHERE session_id=?1 AND kind=?2 ORDER BY seq",
        )?;
        let rows = stmt
            .query_map([session_id, kind], EventRow::from_row)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }
}
