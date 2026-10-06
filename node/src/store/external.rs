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

/// One label's activity on a channel, most recent first.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalLane {
    pub channel: String,
    pub lane: Option<String>,
    pub last_ms: i64,
    pub calls: i64,
    /// Approvals it asked for and reviews it submitted that are still open.
    #[serde(default)]
    pub pending: i64,
}

impl Store {
    /// Every lane that called since `since_ms`, newest first. Only tool calls
    /// count: the log's other entries are outcomes of those calls.
    /// A lane with something still open for the operator is listed however
    /// long ago it last called.
    pub fn external_lanes(&self, since_ms: i64) -> Result<Vec<ExternalLane>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "WITH pending AS (
                 SELECT channel, lane FROM approval
                  WHERE session_id IS NULL AND state = 'pending'
                 UNION ALL
                 SELECT channel, lane FROM review
                  WHERE session_id IS NULL
                    AND state IN ('new', 'claimed', 'revising', 'publishing')
             ),
             open AS (
                 SELECT channel, lane, COUNT(*) AS pending FROM pending
                  GROUP BY channel, lane
             ),
             calls AS (
                 SELECT channel, lane, MAX(at_ms) AS last_ms, COUNT(*) AS calls
                   FROM external_event WHERE kind = 'tool_call'
                  GROUP BY channel, lane
             )
             SELECT c.channel, c.lane, c.last_ms, c.calls, COALESCE(o.pending, 0) AS pending
               FROM calls c
               LEFT JOIN open o ON o.channel = c.channel AND o.lane IS c.lane
              WHERE c.last_ms >= ?1 OR o.pending > 0
              ORDER BY c.last_ms DESC",
        )?;
        let rows = stmt
            .query_map([since_ms], |r| {
                Ok(ExternalLane {
                    channel: r.get("channel")?,
                    lane: r.get("lane")?,
                    last_ms: r.get("last_ms")?,
                    calls: r.get("calls")?,
                    pending: r.get("pending")?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

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

        let lanes = store.external_lanes(0).unwrap();
        assert_eq!(lanes.len(), 2, "{lanes:?}");
        assert_eq!(lanes[0].channel, "personal");
        assert_eq!(lanes[1].lane.as_deref(), Some("tracon:main"));
        assert_eq!(lanes[1].calls, 1);
        assert!(store.external_lanes(3).unwrap().is_empty());
    }

    /// A lane that went quiet still shows while the operator owes it an answer.
    #[test]
    fn a_quiet_lane_with_an_open_approval_is_still_listed() {
        let store = Store::open_in_memory().unwrap();
        store
            .append_external_event(
                "work",
                "n1",
                Some("repo:x"),
                "tool_call",
                None,
                &json!({}),
                1,
            )
            .unwrap();
        assert!(store.external_lanes(10).unwrap().is_empty());
        store
            .insert_approval(&crate::store::approvals::ApprovalRow {
                id: "a1".into(),
                channel: "work".into(),
                session_id: None,
                node_id: "n1".into(),
                lane: Some("repo:x".into()),
                tool: "doc_write".into(),
                arguments: "{}".into(),
                request_key: "k".into(),
                title: "doc_write".into(),
                state: crate::store::approvals::PENDING.into(),
                answer_option_id: None,
                edited_arguments: None,
                result: None,
                reason: None,
                created_ms: 2,
                decided_ms: None,
                finished_ms: None,
                expires_ms: i64::MAX,
            })
            .unwrap();
        let lanes = store.external_lanes(10).unwrap();
        assert_eq!(lanes.len(), 1, "{lanes:?}");
        assert_eq!(lanes[0].lane.as_deref(), Some("repo:x"));
        assert_eq!(lanes[0].pending, 1);
    }
}
