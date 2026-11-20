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

    /// The reviews on `channel` whose target names change `number` of
    /// `project` on `provider`, oldest first. The project is compared
    /// without regard to ASCII case, as the forges do; a target that is not
    /// JSON names nothing.
    pub fn reviews_naming_change(
        &self,
        channel: &str,
        provider: &str,
        project: &str,
        number: u64,
    ) -> Result<Vec<ReviewRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM review
             WHERE channel=?1 AND json_valid(target)
               AND CASE WHEN json_valid(target) THEN
                     json_extract(target, '$.provider')=?2
                     AND lower(json_extract(target, '$.project'))=lower(?3)
                     AND json_extract(target, '$.change.number')=?4
                   END
             ORDER BY created_ms ASC, id ASC",
        )?;
        let number = i64::try_from(number).unwrap_or(i64::MAX);
        let rows = stmt
            .query_map(
                rusqlite::params![channel, provider, project, number],
                ReviewRow::from_row,
            )?
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
