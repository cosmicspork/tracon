//! Approvals: what a caller asked the operator to allow, held until it is
//! decided and, once allowed, what running it produced.
//!
//! An approval is not a permission request. A permission request is a harness
//! waiting on an answer; an approval belongs to the channel, and nothing waits
//! on it: the caller polls `approval_status`, and the node runs the approved
//! call itself. Keeping the two apart keeps every sweep that closes a dead
//! session's requests away from approvals, which outlive any session.

use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use super::{now_ms, PermissionIntent, PermissionRow, PermissionView, Result, Store};

/// Waiting on the operator.
pub const PENDING: &str = "pending";
/// Allowed and being run now.
pub const RUNNING: &str = "running";
pub const SUCCEEDED: &str = "succeeded";
pub const FAILED: &str = "failed";
/// Run, but the remote outcome could not be confirmed.
pub const UNCERTAIN: &str = "uncertain";
pub const REJECTED: &str = "rejected";
pub const EXPIRED: &str = "expired";

/// The answers an approval card offers: the same two a tool card always has.
pub const CARD_OPTIONS: &str = r#"[{"option_id":"allow_once","name":"Allow once","kind":"allow_once"},{"option_id":"reject_once","name":"Reject","kind":"reject_once"}]"#;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRow {
    pub id: String,
    pub channel: String,
    pub session_id: Option<String>,
    pub node_id: String,
    pub lane: Option<String>,
    pub tool: String,
    /// The arguments as asked, as JSON.
    pub arguments: String,
    pub request_key: String,
    pub title: String,
    pub state: String,
    pub answer_option_id: Option<String>,
    /// The operator's rewrite, when they edited the call before allowing it.
    pub edited_arguments: Option<String>,
    /// The tool's result, as JSON, once it ran and succeeded.
    pub result: Option<String>,
    /// Why it was refused, expired, or failed.
    pub reason: Option<String>,
    pub created_ms: i64,
    pub decided_ms: Option<i64>,
    pub finished_ms: Option<i64>,
    pub expires_ms: i64,
}

impl ApprovalRow {
    fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(Self {
            id: r.get("id")?,
            channel: r.get("channel")?,
            session_id: r.get("session_id")?,
            node_id: r.get("node_id")?,
            lane: r.get("lane")?,
            tool: r.get("tool")?,
            arguments: r.get("arguments")?,
            request_key: r.get("request_key")?,
            title: r.get("title")?,
            state: r.get("state")?,
            answer_option_id: r.get("answer_option_id")?,
            edited_arguments: r.get("edited_arguments")?,
            result: r.get("result")?,
            reason: r.get("reason")?,
            created_ms: r.get("created_ms")?,
            decided_ms: r.get("decided_ms")?,
            finished_ms: r.get("finished_ms")?,
            expires_ms: r.get("expires_ms")?,
        })
    }

    /// Decided, and nothing more will happen to it.
    pub fn is_settled(&self) -> bool {
        matches!(
            self.state.as_str(),
            SUCCEEDED | FAILED | UNCERTAIN | REJECTED | EXPIRED
        )
    }

    /// The card the operator's queue shows for it. It reads like the
    /// permission request it replaces, so one card answers both, and its
    /// `raw_input` names the tool and arguments the operator may edit.
    pub fn as_card(&self, options: &str, intent: PermissionIntent) -> PermissionView {
        let arguments: serde_json::Value =
            serde_json::from_str(&self.arguments).unwrap_or(serde_json::Value::Null);
        PermissionView {
            request: PermissionRow {
                id: self.id.clone(),
                session_id: self.session_id.clone(),
                node_id: self.node_id.clone(),
                rpc_id: 0,
                tool_call_id: None,
                title: self.title.clone(),
                kind: Some("tool".into()),
                raw_input: Some(
                    serde_json::json!({
                        "tool": self.tool,
                        "arguments": arguments,
                        "approval_id": self.id,
                        "lane": self.lane,
                    })
                    .to_string(),
                ),
                options: options.to_string(),
                state: "new".into(),
                answer_option_id: None,
                created_ms: self.created_ms,
                created_mono_ms: 0,
                resolved_mono_ms: None,
                expires_ms: self.expires_ms,
            },
            intent,
        }
    }
}

impl Store {
    pub fn insert_approval(&self, a: &ApprovalRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO approval (id, channel, session_id, node_id, lane, tool, arguments,
                request_key, title, state, answer_option_id, edited_arguments, result, reason,
                created_ms, decided_ms, finished_ms, expires_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
            rusqlite::params![
                a.id,
                a.channel,
                a.session_id,
                a.node_id,
                a.lane,
                a.tool,
                a.arguments,
                a.request_key,
                a.title,
                a.state,
                a.answer_option_id,
                a.edited_arguments,
                a.result,
                a.reason,
                a.created_ms,
                a.decided_ms,
                a.finished_ms,
                a.expires_ms
            ],
        )?;
        Ok(())
    }

    pub fn get_approval(&self, id: &str) -> Result<Option<ApprovalRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT * FROM approval WHERE id=?1",
            [id],
            ApprovalRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// The approval already waiting for this same request on this channel.
    pub fn pending_approval(
        &self,
        channel: &str,
        request_key: &str,
    ) -> Result<Option<ApprovalRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT * FROM approval WHERE channel=?1 AND request_key=?2 AND state=?3
             AND expires_ms > ?4 ORDER BY created_ms ASC LIMIT 1",
            rusqlite::params![channel, request_key, PENDING, now_ms()],
            ApprovalRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Move a pending approval to `state` with the operator's answer. False
    /// when it was no longer pending: answered twice, or expired first.
    pub fn decide_approval(
        &self,
        id: &str,
        state: &str,
        option_id: &str,
        edited_arguments: Option<&str>,
        reason: Option<&str>,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let now = now_ms();
        let n = conn.execute(
            "UPDATE approval SET state=?2, answer_option_id=?3, edited_arguments=?4, reason=?5,
                decided_ms=?6, finished_ms=CASE WHEN ?2 IN ('rejected') THEN ?6 ELSE NULL END
             WHERE id=?1 AND state='pending' AND expires_ms > ?6",
            rusqlite::params![id, state, option_id, edited_arguments, reason, now],
        )?;
        Ok(n == 1)
    }

    /// Record what running an approved call produced.
    pub fn finish_approval(
        &self,
        id: &str,
        state: &str,
        result: Option<&str>,
        reason: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE approval SET state=?2, result=?3, reason=?4, finished_ms=?5
             WHERE id=?1 AND state='running'",
            rusqlite::params![id, state, result, reason, now_ms()],
        )?;
        Ok(())
    }

    /// Expire every pending approval whose time is up, returning them.
    pub fn expire_due_approvals(&self) -> Result<Vec<ApprovalRow>> {
        let conn = self.conn.lock().unwrap();
        let now = now_ms();
        let due: Vec<ApprovalRow> = {
            let mut stmt =
                conn.prepare("SELECT * FROM approval WHERE state='pending' AND expires_ms <= ?1")?;
            let rows = stmt
                .query_map([now], ApprovalRow::from_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows
        };
        let mut expired = Vec::new();
        for mut a in due {
            let n = conn.execute(
                "UPDATE approval SET state='expired', reason=?2, finished_ms=?3
                 WHERE id=?1 AND state='pending'",
                rusqlite::params![a.id, "unanswered before it expired", now],
            )?;
            if n == 1 {
                a.state = EXPIRED.into();
                a.reason = Some("unanswered before it expired".into());
                a.finished_ms = Some(now);
                expired.push(a);
            }
        }
        Ok(expired)
    }

    /// Pending approvals as the cards the operator's queue shows, each with
    /// the intent of the session that asked.
    pub fn open_approval_cards(&self, options: &str) -> Result<Vec<PermissionView>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT a.*, s.channel AS intent_channel, s.phase AS intent_phase,
                    s.branch AS intent_branch, s.state AS intent_session_state,
                    s.work_item_id AS intent_work_item_id,
                    w.title AS intent_work_item_title
             FROM approval a
             LEFT JOIN session s ON s.id = a.session_id
             LEFT JOIN work_item w ON w.id = s.work_item_id AND w.deleted = 0
             WHERE a.state='pending' AND a.expires_ms > ?1 ORDER BY a.created_ms ASC",
        )?;
        let rows = stmt
            .query_map([now_ms()], |r| {
                let approval = ApprovalRow::from_row(r)?;
                let intent = PermissionIntent {
                    channel: r
                        .get::<_, Option<String>>("intent_channel")?
                        .or_else(|| Some(approval.channel.clone())),
                    phase: r.get("intent_phase")?,
                    branch: r.get("intent_branch")?,
                    work_item_id: r.get("intent_work_item_id")?,
                    work_item_title: r.get("intent_work_item_title")?,
                    session_state: r.get("intent_session_state")?,
                };
                Ok(approval.as_card(options, intent))
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, key: &str, expires_ms: i64) -> ApprovalRow {
        ApprovalRow {
            id: id.into(),
            channel: "work".into(),
            session_id: Some("s1".into()),
            node_id: "n1".into(),
            lane: None,
            tool: "doc_write".into(),
            arguments: r#"{"slug":"plan-x","body":"b"}"#.into(),
            request_key: key.into(),
            title: "doc_write plan-x".into(),
            state: PENDING.into(),
            answer_option_id: None,
            edited_arguments: None,
            result: None,
            reason: None,
            created_ms: now_ms(),
            decided_ms: None,
            finished_ms: None,
            expires_ms,
        }
    }

    #[test]
    fn a_pending_approval_is_found_by_its_request_and_decided_once() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert_approval(&row("a1", "k", now_ms() + 60_000))
            .unwrap();
        assert_eq!(
            store.pending_approval("work", "k").unwrap().unwrap().id,
            "a1"
        );
        assert!(store.pending_approval("personal", "k").unwrap().is_none());
        assert!(store
            .decide_approval("a1", RUNNING, "allow_once", None, None)
            .unwrap());
        assert!(!store
            .decide_approval("a1", REJECTED, "reject_once", None, None)
            .unwrap());
        assert!(store.pending_approval("work", "k").unwrap().is_none());
        store
            .finish_approval("a1", SUCCEEDED, Some("{}"), None)
            .unwrap();
        assert!(store.get_approval("a1").unwrap().unwrap().is_settled());
    }

    #[test]
    fn an_unanswered_approval_expires_and_cannot_then_be_decided() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert_approval(&row("a1", "k", now_ms() - 1))
            .unwrap();
        assert!(store.open_approval_cards("[]").unwrap().is_empty());
        let expired = store.expire_due_approvals().unwrap();
        assert_eq!(expired.len(), 1);
        assert_eq!(store.get_approval("a1").unwrap().unwrap().state, EXPIRED);
        assert!(!store
            .decide_approval("a1", RUNNING, "allow_once", None, None)
            .unwrap());
    }
}
