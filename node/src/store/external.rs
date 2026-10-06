//! What harnesses the operator runs themselves asked of a channel. Those
//! callers have no session, so their log is the channel's, and each entry
//! carries the lane the caller labelled itself with.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Result, Store};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalEventRow {
    pub seq: i64,
    pub channel: String,
    pub node_id: String,
    pub lane: Option<String>,
    pub kind: String,
    pub ref_id: Option<String>,
    pub payload: Value,
    pub at_ms: i64,
}

impl ExternalEventRow {
    fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Self> {
        let payload: String = r.get("payload")?;
        Ok(Self {
            seq: r.get("seq")?,
            channel: r.get("channel")?,
            node_id: r.get("node_id")?,
            lane: r.get("lane")?,
            kind: r.get("kind")?,
            ref_id: r.get("ref_id")?,
            payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
            at_ms: r.get("at_ms")?,
        })
    }
}

impl Store {
    /// Append to a channel's external log; the row as stored, with its seq.
    #[allow(clippy::too_many_arguments)]
    pub fn append_external_event(
        &self,
        channel: &str,
        node_id: &str,
        lane: Option<&str>,
        kind: &str,
        ref_id: Option<&str>,
        payload: &Value,
        at_ms: i64,
    ) -> Result<ExternalEventRow> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO external_event (channel, node_id, lane, kind, ref_id, payload, at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                channel,
                node_id,
                lane,
                kind,
                ref_id,
                serde_json::to_string(payload)?,
                at_ms
            ],
        )?;
        Ok(ExternalEventRow {
            seq: conn.last_insert_rowid(),
            channel: channel.to_string(),
            node_id: node_id.to_string(),
            lane: lane.map(str::to_string),
            kind: kind.to_string(),
            ref_id: ref_id.map(str::to_string),
            payload: payload.clone(),
            at_ms,
        })
    }

    pub fn external_events_after(
        &self,
        channel: &str,
        after_seq: i64,
        limit: i64,
    ) -> Result<Vec<ExternalEventRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM external_event WHERE channel = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(
                rusqlite::params![channel, after_seq, limit],
                ExternalEventRow::from_row,
            )?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_channels_external_log_reads_back_in_order_with_its_lanes() {
        let store = Store::open_in_memory().unwrap();
        let first = store
            .append_external_event(
                "work",
                "n1",
                Some("tracon:main"),
                "tool_call",
                None,
                &json!({ "title": "recall" }),
                1,
            )
            .unwrap();
        store
            .append_external_event("personal", "n1", None, "tool_call", None, &json!({}), 2)
            .unwrap();
        store
            .append_external_event(
                "work",
                "n1",
                None,
                "tool_result",
                None,
                &json!({ "status": "ok" }),
                3,
            )
            .unwrap();
        let work = store.external_events_after("work", 0, 10).unwrap();
        assert_eq!(work.len(), 2);
        assert_eq!(work[0].seq, first.seq);
        assert_eq!(work[0].lane.as_deref(), Some("tracon:main"));
        assert_eq!(work[0].payload["title"], "recall");
        assert_eq!(work[1].kind, "tool_result");
        assert!(store
            .external_events_after("work", work[1].seq, 10)
            .unwrap()
            .is_empty());
    }
}
