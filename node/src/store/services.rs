//! The services a session started, read back from its events.

use rusqlite::params;
use serde_json::Value;

use super::{Result, Store};

impl Store {
    /// Each `service` event's payload for a session, oldest first.
    pub fn session_service_events(&self, session_id: &str) -> Result<Vec<(Value, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT payload, at_ms FROM event WHERE session_id=?1 AND kind='service' ORDER BY seq",
        )?;
        let rows = stmt
            .query_map(params![session_id], |row| {
                let payload: String = row.get(0)?;
                Ok((
                    serde_json::from_str(&payload).unwrap_or(Value::Null),
                    row.get::<_, i64>(1)?,
                ))
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }
}
