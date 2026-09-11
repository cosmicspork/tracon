use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::{Result, Store};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowMetrics {
    pub setup_failures: i64,
    pub verified_sessions: i64,
    pub seconds_to_first_verified_candidate: Option<f64>,
    pub interventions: i64,
    pub question_wait_seconds: f64,
    pub human_wait_seconds: f64,
}

impl Store {
    pub fn workflow_metrics(&self, channel: &str, since_ms: i64) -> Result<WorkflowMetrics> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| super::StoreError::Invalid("store lock poisoned".into()))?;
        let setup_failures = conn.query_row(
            "SELECT COUNT(*) FROM session s
             WHERE s.channel = ?1 AND s.created_ms >= ?2 AND s.state = 'failed'
               AND NOT EXISTS (SELECT 1 FROM event e WHERE e.session_id = s.id AND e.kind = 'session_started')",
            params![channel, since_ms],
            |r| r.get(0),
        )?;
        let (verified_sessions, seconds_to_first_verified_candidate) = conn.query_row(
            "SELECT COUNT(*), AVG(MAX(first_verified_ms - created_ms, 0)) / 1000.0
             FROM (
                 SELECT s.id, s.created_ms, MIN(e.at_ms) AS first_verified_ms
                 FROM session s JOIN event e ON e.session_id = s.id
                 WHERE s.channel = ?1 AND s.created_ms >= ?2 AND e.kind = 'candidate_verified'
                 GROUP BY s.id
             )",
            params![channel, since_ms],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let (interventions, question_wait_seconds, review_wait_seconds) = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(CASE WHEN e.kind = 'operator_question_answered'
                 THEN MAX(COALESCE(json_extract(e.payload, '$.waiting_ms'), 0), 0) ELSE 0 END), 0) / 1000.0,
                 COALESCE(SUM(CASE WHEN e.kind = 'review_decision'
                 THEN MAX(COALESCE(json_extract(e.payload, '$.waiting_ms'), 0), 0) ELSE 0 END), 0) / 1000.0
             FROM event e JOIN session s ON s.id = e.session_id
             WHERE s.channel = ?1 AND e.at_ms >= ?2 AND (
                 e.kind = 'operator_question_answered' OR
                 (e.kind IN ('session_paused', 'session_resumed', 'review_decision') AND json_extract(e.payload, '$.source') = 'operator')
             )",
            params![channel, since_ms],
            |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, f64>(2)?)),
        )?;
        Ok(WorkflowMetrics {
            setup_failures,
            verified_sessions,
            seconds_to_first_verified_candidate,
            interventions,
            question_wait_seconds,
            human_wait_seconds: review_wait_seconds + question_wait_seconds,
        })
    }
}
