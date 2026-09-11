use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::{Result, Store, now_ms};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorQuestionRow {
    pub id: String,
    pub session_id: String,
    pub channel: String,
    pub node_id: String,
    pub prompt: String,
    pub choices_json: String,
    pub state: String,
    pub answer_json: Option<String>,
    pub created_ms: i64,
    pub answered_ms: Option<i64>,
}

impl OperatorQuestionRow {
    fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: r.get("id")?, session_id: r.get("session_id")?, channel: r.get("channel")?,
            node_id: r.get("node_id")?, prompt: r.get("prompt")?, choices_json: r.get("choices_json")?,
            state: r.get("state")?, answer_json: r.get("answer_json")?, created_ms: r.get("created_ms")?,
            answered_ms: r.get("answered_ms")?,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueDraftRow {
    pub id: String,
    pub session_id: String,
    pub channel: String,
    pub title: String,
    pub body: String,
    pub attachments_json: String,
    pub state: String,
    pub published_url: Option<String>,
    pub publish_error: Option<String>,
    pub created_ms: i64,
    pub approved_ms: Option<i64>,
}

impl IssueDraftRow {
    fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: r.get("id")?, session_id: r.get("session_id")?, channel: r.get("channel")?,
            title: r.get("title")?, body: r.get("body")?, attachments_json: r.get("attachments_json")?,
            state: r.get("state")?, published_url: r.get("published_url")?, publish_error: r.get("publish_error")?,
            created_ms: r.get("created_ms")?, approved_ms: r.get("approved_ms")?,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationAttemptRow {
    pub id: String,
    pub notification_id: String,
    pub device_id: String,
    pub outcome: String,
    pub attempted_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorNotificationRow {
    pub id: String,
    pub expires_ms: i64,
}

impl Store {
    pub fn insert_operator_question(&self, row: &OperatorQuestionRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("INSERT INTO operator_question (id, session_id, channel, node_id, prompt, choices_json, state, answer_json, created_ms, answered_ms) VALUES (?1,?2,?3,?4,?5,?6,'unanswered',NULL,?7,NULL)", params![row.id,row.session_id,row.channel,row.node_id,row.prompt,row.choices_json,row.created_ms])?;
        Ok(())
    }
    pub fn operator_question(&self, id: &str) -> Result<Option<OperatorQuestionRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT * FROM operator_question WHERE id=?1", [id], OperatorQuestionRow::from_row).optional().map_err(Into::into)
    }
    pub fn open_operator_questions(&self) -> Result<Vec<OperatorQuestionRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM operator_question WHERE state='unanswered' ORDER BY created_ms")?;
        let rows = stmt.query_map([], OperatorQuestionRow::from_row)?.collect::<std::result::Result<_,_>>()?;
        Ok(rows)
    }
    pub fn session_operator_questions(&self, session_id: &str) -> Result<Vec<OperatorQuestionRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM operator_question WHERE session_id=?1 AND state='unanswered' ORDER BY created_ms")?;
        let rows = stmt.query_map([session_id], OperatorQuestionRow::from_row)?.collect::<std::result::Result<_,_>>()?;
        Ok(rows)
    }
    /// Answer and record the intervention in one transaction. A duplicate
    /// answer cannot create a second metrics event.
    pub fn answer_operator_question(&self, id: &str, answer_json: &str) -> Result<Option<(String, i64)>> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let row = tx.query_row(
            "SELECT session_id, created_ms FROM operator_question WHERE id=?1 AND state='unanswered'",
            [id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        ).optional()?;
        let Some((session_id, created_ms)) = row else { return Ok(None); };
        let now = now_ms();
        tx.execute(
            "UPDATE operator_question SET state='answered', answer_json=?2, answered_ms=?3 WHERE id=?1",
            params![id, answer_json, now],
        )?;
        tx.execute(
            "INSERT INTO event (session_id, work_item_id, kind, ref_id, payload, at_ms, mono_ms, node_id)
             VALUES (?1, (SELECT work_item_id FROM session WHERE id=?1), 'operator_question_answered', ?2, ?3, ?4, 0, (SELECT node_id FROM session WHERE id=?1))",
            params![session_id, id, serde_json::json!({"question_id": id, "waiting_ms": (now - created_ms).max(0)}).to_string(), now],
        )?;
        tx.commit()?;
        Ok(Some((session_id, created_ms)))
    }
    pub fn cancel_operator_question(&self, id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("UPDATE operator_question SET state='cancelled', answered_ms=?2 WHERE id=?1 AND state='unanswered'", params![id, now_ms()])? == 1)
    }
    pub fn insert_issue_draft(&self, row: &IssueDraftRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("INSERT INTO operator_issue (id,session_id,channel,title,body,attachments_json,state,published_url,publish_error,created_ms,approved_ms) VALUES (?1,?2,?3,?4,?5,?6,'draft',NULL,NULL,?7,NULL)", params![row.id,row.session_id,row.channel,row.title,row.body,row.attachments_json,row.created_ms])?;
        Ok(())
    }
    pub fn issue_drafts(&self, open_only: bool) -> Result<Vec<IssueDraftRow>> {
        let conn = self.conn.lock().unwrap();
        let sql = if open_only { "SELECT * FROM operator_issue WHERE state IN ('draft','publishing') ORDER BY created_ms" } else { "SELECT * FROM operator_issue ORDER BY created_ms DESC" };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([], IssueDraftRow::from_row)?.collect::<std::result::Result<_,_>>()?;
        Ok(rows)
    }
    pub fn issue_draft(&self, id: &str) -> Result<Option<IssueDraftRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT * FROM operator_issue WHERE id=?1", [id], IssueDraftRow::from_row).optional().map_err(Into::into)
    }
    pub fn begin_issue_publication(&self, id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("UPDATE operator_issue SET state='publishing', approved_ms=?2 WHERE id=?1 AND state='draft'", params![id, now_ms()])? == 1)
    }
    pub fn finish_issue_publication(&self, id: &str, url: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap(); conn.execute("UPDATE operator_issue SET state='published', published_url=?2, publish_error=NULL WHERE id=?1 AND state='publishing'", params![id,url])?; Ok(())
    }
    pub fn fail_issue_publication(&self, id: &str, error: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap(); conn.execute("UPDATE operator_issue SET state='draft', publish_error=?2 WHERE id=?1 AND state='publishing'", params![id,error])?; Ok(())
    }
    pub fn record_notification_attempt(&self, notification_id: &str, device_id: &str, outcome: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap(); conn.execute("INSERT INTO operator_notification_attempt (id,notification_id,device_id,outcome,attempted_ms) VALUES (?1,?2,?3,?4,?5)", params![uuid::Uuid::now_v7().to_string(),notification_id,device_id,outcome,now_ms()])?; Ok(())
    }
    pub fn notification_attempts(&self, notification_id: &str) -> Result<Vec<NotificationAttemptRow>> {
        let conn = self.conn.lock().unwrap(); let mut stmt=conn.prepare("SELECT * FROM operator_notification_attempt WHERE notification_id=?1 ORDER BY attempted_ms")?; let rows = stmt.query_map([notification_id], |r| Ok(NotificationAttemptRow{id:r.get("id")?,notification_id:r.get("notification_id")?,device_id:r.get("device_id")?,outcome:r.get("outcome")?,attempted_ms:r.get("attempted_ms")?}))?.collect::<std::result::Result<_,_>>()?;
        Ok(rows)
    }
    /// Dedup is durable: two MCP calls with the same content in the window send once.

    pub fn operator_notifications(&self) -> Result<Vec<OperatorNotificationRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id, expires_ms FROM operator_notification ORDER BY expires_ms DESC")?;
        let rows = stmt.query_map([], |r| Ok(OperatorNotificationRow { id: r.get(0)?, expires_ms: r.get(1)? }))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// A durable per-origin/channel fixed window. Deduplicated calls never
    /// reach here, so a body change cannot evade the quota.
    pub fn claim_operator_notification_rate(
        &self,
        channel: &str,
        origin: &str,
        limit: i64,
        window_ms: i64,
    ) -> Result<bool> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let now = now_ms();
        let existing: Option<(i64, i64)> = tx
            .query_row(
                "SELECT window_started_ms, count FROM operator_notification_rate WHERE channel=?1 AND origin=?2",
                params![channel, origin],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let allowed = match existing {
            Some((started, count)) if now - started < window_ms => {
                if count >= limit {
                    false
                } else {
                    tx.execute(
                        "UPDATE operator_notification_rate SET count=count+1 WHERE channel=?1 AND origin=?2",
                        params![channel, origin],
                    )?;
                    true
                }
            }
            _ => {
                tx.execute(
                    "INSERT INTO operator_notification_rate (channel,origin,window_started_ms,count)
                     VALUES (?1,?2,?3,1)
                     ON CONFLICT(channel,origin) DO UPDATE SET window_started_ms=excluded.window_started_ms,count=1",
                    params![channel, origin, now],
                )?;
                true
            }
        };
        tx.commit()?;
        Ok(allowed)
    }
    pub fn claim_operator_notification(&self, dedup_key: &str, id: &str, ttl_ms: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap(); let now=now_ms();
        conn.execute("DELETE FROM operator_notification WHERE expires_ms <= ?1", [now])?;
        Ok(conn.execute("INSERT OR IGNORE INTO operator_notification (dedup_key,id,expires_ms) VALUES (?1,?2,?3)", params![dedup_key,id,now+ttl_ms])? == 1)
    }

    pub fn release_operator_notification(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM operator_notification WHERE id=?1", [id])?;
        Ok(())
    }
}
