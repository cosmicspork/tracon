//! Pipelines an agent started, followed until they finish.
//!
//! `pipeline_run`, `job_play` and `deploy` subscribe their caller to the
//! pipeline they start; the node reads it on the same tick it follows
//! published requests (`crate::follow`), so a pipeline outlives whatever
//! triggered it. A row is the subscription and the last thing seen of it.

use rusqlite::{params, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use super::{Result, Store};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineFollowRow {
    pub id: String,
    pub node_id: String,
    pub channel: String,
    /// The session that started it; none for a harness the operator runs.
    pub session_id: Option<String>,
    /// What an external harness labels itself, for display. Never authority.
    pub lane: Option<String>,
    pub provider: String,
    pub project: String,
    pub pipeline_id: i64,
    /// The tool whose call started it.
    pub started_by: String,
    pub web_url: Option<String>,
    /// What the node saw last; none before the first look.
    pub snapshot: Option<serde_json::Value>,
    pub done: bool,
    pub created_ms: i64,
    /// When the pipeline last moved, or when it was followed again.
    pub changed_ms: i64,
}

impl PipelineFollowRow {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        let snapshot: Option<String> = row.get("snapshot")?;
        Ok(Self {
            id: row.get("id")?,
            node_id: row.get("node_id")?,
            channel: row.get("channel")?,
            session_id: row.get("session_id")?,
            lane: row.get("lane")?,
            provider: row.get("provider")?,
            project: row.get("project")?,
            pipeline_id: row.get("pipeline_id")?,
            started_by: row.get("started_by")?,
            web_url: row.get("web_url")?,
            snapshot: snapshot.and_then(|s| serde_json::from_str(&s).ok()),
            done: row.get::<_, i64>("done")? != 0,
            created_ms: row.get("created_ms")?,
            changed_ms: row.get("changed_ms")?,
        })
    }
}

/// Who started which pipeline, and what it looked like right after.
pub struct NewPipelineFollow<'a> {
    pub node_id: &'a str,
    pub channel: &'a str,
    pub session_id: Option<&'a str>,
    pub lane: Option<&'a str>,
    pub provider: &'a str,
    pub project: &'a str,
    pub pipeline_id: i64,
    pub started_by: &'a str,
    pub web_url: Option<&'a str>,
    pub snapshot: Option<&'a serde_json::Value>,
    pub now_ms: i64,
}

impl Store {
    /// Subscribe a caller to a pipeline. The same caller starting work on a
    /// pipeline it already follows (playing a manual job it stopped at)
    /// follows it again from what it looks like now, not a second time.
    pub fn follow_pipeline(&self, f: &NewPipelineFollow) -> Result<String> {
        let conn = self.conn.lock().unwrap();
        let snapshot = f.snapshot.map(serde_json::to_string).transpose()?;
        let existing: Option<String> = conn
            .query_row(
                "SELECT id FROM pipeline_follow
                  WHERE node_id=?1 AND channel=?2 AND session_id IS ?3 AND lane IS ?4
                    AND provider=?5 AND project=?6 AND pipeline_id=?7",
                params![
                    f.node_id,
                    f.channel,
                    f.session_id,
                    f.lane,
                    f.provider,
                    f.project,
                    f.pipeline_id
                ],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            conn.execute(
                "UPDATE pipeline_follow
                    SET done=0, started_by=?2, web_url=COALESCE(?3, web_url),
                        snapshot=COALESCE(?4, snapshot), changed_ms=?5
                  WHERE id=?1",
                params![id, f.started_by, f.web_url, snapshot, f.now_ms],
            )?;
            return Ok(id);
        }
        let id = uuid::Uuid::now_v7().to_string();
        conn.execute(
            "INSERT INTO pipeline_follow (id, node_id, channel, session_id, lane, provider,
                 project, pipeline_id, started_by, web_url, snapshot, done, created_ms, changed_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,0,?12,?12)",
            params![
                id,
                f.node_id,
                f.channel,
                f.session_id,
                f.lane,
                f.provider,
                f.project,
                f.pipeline_id,
                f.started_by,
                f.web_url,
                snapshot,
                f.now_ms
            ],
        )?;
        Ok(id)
    }

    /// The pipelines this node still follows, oldest first.
    pub fn pipelines_followed(&self, node_id: &str) -> Result<Vec<PipelineFollowRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM pipeline_follow WHERE node_id=?1 AND done=0 ORDER BY created_ms",
        )?;
        let rows = stmt
            .query_map([node_id], PipelineFollowRow::from_row)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    pub fn pipeline_follow(&self, id: &str) -> Result<Option<PipelineFollowRow>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT * FROM pipeline_follow WHERE id=?1",
                [id],
                PipelineFollowRow::from_row,
            )
            .optional()?)
    }

    /// What one look saw. `moved` says whether it differs from the last.
    pub fn pipeline_follow_seen(
        &self,
        id: &str,
        snapshot: &serde_json::Value,
        moved: bool,
        done: bool,
        now_ms: i64,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE pipeline_follow
                SET snapshot=?2, done=?3,
                    changed_ms=CASE WHEN ?4 THEN ?5 ELSE changed_ms END
              WHERE id=?1",
            params![id, serde_json::to_string(snapshot)?, done, moved, now_ms],
        )?;
        Ok(())
    }

    /// Stop following without a look: it has not moved for too long.
    pub fn pipeline_follow_drop(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE pipeline_follow SET done=1 WHERE id=?1", [id])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn new<'a>(session: Option<&'a str>, by: &'a str, now: i64) -> NewPipelineFollow<'a> {
        NewPipelineFollow {
            node_id: "n1",
            channel: "personal",
            session_id: session,
            lane: None,
            provider: "gitlab",
            project: "g/p",
            pipeline_id: 9,
            started_by: by,
            web_url: Some("https://gitlab.test/g/p/-/pipelines/9"),
            snapshot: None,
            now_ms: now,
        }
    }

    #[test]
    fn a_pipeline_is_followed_once_per_caller_and_again_when_played() {
        let store = Store::open_in_memory().unwrap();
        let id = store
            .follow_pipeline(&new(None, "pipeline_run", 1))
            .unwrap();
        assert_eq!(store.pipelines_followed("n1").unwrap().len(), 1);
        let seen = json!({ "status": "manual", "jobs": {} });
        store
            .pipeline_follow_seen(&id, &seen, true, true, 2)
            .unwrap();
        assert!(store.pipelines_followed("n1").unwrap().is_empty());
        // Playing the manual job it stopped at follows the same row again.
        let again = store.follow_pipeline(&new(None, "job_play", 3)).unwrap();
        assert_eq!(again, id);
        let row = store.pipeline_follow(&id).unwrap().unwrap();
        assert!(!row.done);
        assert_eq!(row.started_by, "job_play");
        assert_eq!(row.snapshot, Some(seen));
        // Another caller follows it on its own.
        store
            .follow_pipeline(&new(Some("s1"), "deploy", 4))
            .unwrap();
        assert_eq!(store.pipelines_followed("n1").unwrap().len(), 2);
        assert!(store.pipelines_followed("n2").unwrap().is_empty());
    }
}
