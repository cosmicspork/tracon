//! Aggregate mesh rollups. These are intentionally separate from local
//! metrics: a hub may be absent forever, while the node's own accounting must
//! remain complete and immediately usable.

use std::collections::BTreeMap;

use proto::frame::Rollup;
use rusqlite::{params, OptionalExtension};

use super::{now_ms, Result, Store};

impl Store {
    /// Build the smallest useful channel summary and advance its persisted
    /// sequence. No transcript, prompt, path, credential, or record body is
    /// selected here. A sequence is allocated only after every count succeeds,
    /// so an accepted rollup always describes one complete local read.
    pub fn next_rollup(&self, node_id: &str, channel: &str) -> Result<Rollup> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        let mut session_counts = BTreeMap::new();
        let mut states = tx.prepare(
            "SELECT state, COUNT(*) FROM session WHERE node_id = ?1 AND channel = ?2 GROUP BY state",
        )?;
        let rows = states.query_map(params![node_id, channel], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (state, count) = row?;
            session_counts.insert(state, count.max(0) as u64);
        }
        drop(states);

        let queued_permissions: i64 = tx.query_row(
            "SELECT COUNT(*) FROM permission_request p
             JOIN session s ON s.id = p.session_id
             WHERE s.node_id = ?1 AND s.channel = ?2 AND p.state = 'new'",
            params![node_id, channel],
            |row| row.get(0),
        )?;
        let queued_reviews: i64 = tx.query_row(
            "SELECT COUNT(*) FROM review WHERE node_id = ?1 AND channel = ?2
             AND state IN ('new', 'claimed', 'revising')",
            params![node_id, channel],
            |row| row.get(0),
        )?;
        let work_items: i64 = tx.query_row(
            "SELECT COUNT(*) FROM work_item WHERE channel = ?1 AND deleted = 0",
            [channel],
            |row| row.get(0),
        )?;
        let documents: i64 = tx.query_row(
            "SELECT COUNT(*) FROM document WHERE channel = ?1 AND deleted = 0",
            [channel],
            |row| row.get(0),
        )?;
        let memories: i64 = tx.query_row(
            "SELECT COUNT(*) FROM memory WHERE channel = ?1 AND deleted = 0",
            [channel],
            |row| row.get(0),
        )?;
        let last: Option<i64> = tx
            .query_row(
                "SELECT seq FROM mesh_rollup_seq WHERE channel = ?1",
                [channel],
                |row| row.get(0),
            )
            .optional()?;
        let seq = last.unwrap_or(0).checked_add(1).ok_or_else(|| {
            super::StoreError::Invalid("rollup sequence exhausted".into())
        })?;
        tx.execute(
            "INSERT INTO mesh_rollup_seq (channel, seq) VALUES (?1, ?2)
             ON CONFLICT(channel) DO UPDATE SET seq = excluded.seq",
            params![channel, seq],
        )?;
        tx.commit()?;

        Ok(Rollup {
            node_id: node_id.to_string(),
            seq: seq as u64,
            captured_ms: now_ms(),
            complete: true,
            session_counts,
            queued_permissions: queued_permissions.max(0) as u64,
            queued_reviews: queued_reviews.max(0) as u64,
            work_items: work_items.max(0) as u64,
            documents: documents.max(0) as u64,
            memories: memories.max(0) as u64,
        })
    }
}
