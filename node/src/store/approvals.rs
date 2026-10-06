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
/// Not run: the operator wants the caller to revise it and ask again.
pub const CHANGES_REQUESTED: &str = "changes_requested";
pub const EXPIRED: &str = "expired";

/// The option an operator picks to send a call back for revision.
pub const OPTION_REQUEST_CHANGES: &str = "request_changes";

/// The answers an approval card offers: the two a tool card always has, and
/// a request for changes, which a client that does not know its kind skips.
pub const CARD_OPTIONS: &str = r#"[{"option_id":"allow_once","name":"Allow once","kind":"allow_once"},{"option_id":"reject_once","name":"Reject","kind":"reject_once"},{"option_id":"request_changes","name":"Request changes","kind":"request_changes"}]"#;

/// The approval a session's refused connection, or its `request_egress`,
/// puts to the operator. Its arguments name the host; allowing it opens that
/// host to the session's own grant rather than running a call.
pub const EGRESS_TOOL: &str = "request_egress";

/// Open the host for the rest of the session. ("Allow once" opens it for
/// the retry the refusal asked for, and no longer.)
pub const OPTION_ALLOW_SESSION: &str = "allow_session";
/// Open it for the rest of the session and add it to the repository's
/// `egress`, so the next session starts with it.
pub const OPTION_ALLOW_REPO: &str = "allow_repo";

/// What an egress card offers: how long the host stays open, or no.
pub const EGRESS_CARD_OPTIONS: &str = r#"[{"option_id":"allow_once","name":"Allow once","kind":"allow_once"},{"option_id":"allow_session","name":"For this session","kind":"allow_session"},{"option_id":"allow_repo","name":"Save to the repository's egress","kind":"allow_repo"},{"option_id":"reject_once","name":"Reject","kind":"reject_once"}]"#;

/// The answers a card for `tool` offers.
pub fn card_options(tool: &str) -> &'static str {
    if tool == EGRESS_TOOL {
        EGRESS_CARD_OPTIONS
    } else {
        CARD_OPTIONS
    }
}

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
    /// What the operator said back with their answer: the notes on a request
    /// for changes, or a remark with an allow.
    pub operator_note: Option<String>,
    /// When an operator last opened it, while it waits.
    pub claimed_ms: Option<i64>,
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
            operator_note: r.get("operator_note")?,
            claimed_ms: r.get("claimed_ms")?,
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
            SUCCEEDED | FAILED | UNCERTAIN | REJECTED | CHANGES_REQUESTED | EXPIRED
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

/// The operator's answer to a pending approval, as it is recorded.
#[derive(Debug, Default)]
pub struct Decision<'a> {
    pub state: &'a str,
    pub option_id: &'a str,
    pub edited_arguments: Option<&'a str>,
    pub reason: Option<&'a str>,
    pub note: Option<&'a str>,
}

impl Store {
    pub fn insert_approval(&self, a: &ApprovalRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO approval (id, channel, session_id, node_id, lane, tool, arguments,
                request_key, title, state, answer_option_id, edited_arguments, result, reason,
                operator_note, claimed_ms, created_ms, decided_ms, finished_ms, expires_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",
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
                a.operator_note,
                a.claimed_ms,
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

    /// The newest approval for this request on this channel, in any state:
    /// whether it was already asked, and what became of it.
    pub fn latest_approval(&self, channel: &str, request_key: &str) -> Result<Option<ApprovalRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT * FROM approval WHERE channel=?1 AND request_key=?2
             ORDER BY created_ms DESC LIMIT 1",
            rusqlite::params![channel, request_key],
            ApprovalRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Move a pending approval to `state` with the operator's answer. False
    /// when it was no longer pending: answered twice, or expired first.
    pub fn decide_approval(&self, id: &str, decision: &Decision<'_>) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let now = now_ms();
        let n = conn.execute(
            "UPDATE approval SET state=?2, answer_option_id=?3, edited_arguments=?4, reason=?5,
                operator_note=?6, claimed_ms=NULL, decided_ms=?7,
                finished_ms=CASE WHEN ?2 IN ('rejected','changes_requested') THEN ?7 ELSE NULL END
             WHERE id=?1 AND state='pending' AND expires_ms > ?7",
            rusqlite::params![
                id,
                decision.state,
                decision.option_id,
                decision.edited_arguments,
                decision.reason,
                decision.note,
                now
            ],
        )?;
        Ok(n == 1)
    }

    /// Mark a waiting approval as open in front of an operator. Each open
    /// refreshes it, so the sweeper lapses only one nobody is looking at.
    pub fn claim_approval(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE approval SET claimed_ms=?2 WHERE id=?1 AND state='pending'",
            rusqlite::params![id, now_ms()],
        )?;
        Ok(())
    }

    pub fn release_approval(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE approval SET claimed_ms=NULL WHERE id=?1 AND claimed_ms IS NOT NULL",
            [id],
        )?;
        Ok(())
    }

    /// Claims older than the grace period, for the sweeper.
    pub fn stale_approval_claims(&self, older_than_ms: i64) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id FROM approval WHERE claimed_ms < ?1")?;
        let rows = stmt
            .query_map([now_ms() - older_than_ms], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
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
    /// `options` is what a card offers unless its tool has its own
    /// ([`card_options`]).
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
                let options = match approval.tool.as_str() {
                    EGRESS_TOOL => EGRESS_CARD_OPTIONS,
                    _ => options,
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
            operator_note: None,
            claimed_ms: None,
            created_ms: now_ms(),
            decided_ms: None,
            finished_ms: None,
            expires_ms,
        }
    }

    fn allow() -> Decision<'static> {
        Decision {
            state: RUNNING,
            option_id: "allow_once",
            ..Default::default()
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
        assert!(store.decide_approval("a1", &allow()).unwrap());
        assert!(!store
            .decide_approval(
                "a1",
                &Decision {
                    state: REJECTED,
                    option_id: "reject_once",
                    ..Default::default()
                }
            )
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
        assert!(!store.decide_approval("a1", &allow()).unwrap());
    }

    #[test]
    fn a_request_for_changes_settles_it_and_keeps_the_notes_past_a_run() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert_approval(&row("a1", "k", now_ms() + 60_000))
            .unwrap();
        assert!(store
            .decide_approval(
                "a1",
                &Decision {
                    state: CHANGES_REQUESTED,
                    option_id: OPTION_REQUEST_CHANGES,
                    note: Some("split the second section"),
                    ..Default::default()
                }
            )
            .unwrap());
        let a = store.get_approval("a1").unwrap().unwrap();
        assert!(a.is_settled());
        assert!(a.finished_ms.is_some());
        assert_eq!(a.operator_note.as_deref(), Some("split the second section"));
        assert!(store.pending_approval("work", "k").unwrap().is_none());

        store
            .insert_approval(&row("a2", "k2", now_ms() + 60_000))
            .unwrap();
        let allowed = Decision {
            note: Some("ship it"),
            ..allow()
        };
        assert!(store.decide_approval("a2", &allowed).unwrap());
        store
            .finish_approval("a2", FAILED, None, Some("jira said no"))
            .unwrap();
        let a = store.get_approval("a2").unwrap().unwrap();
        assert_eq!(a.reason.as_deref(), Some("jira said no"));
        assert_eq!(a.operator_note.as_deref(), Some("ship it"));
    }

    #[test]
    fn a_claim_lapses_when_nobody_refreshes_it_and_ends_with_the_decision() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert_approval(&row("a1", "k", now_ms() + 60_000))
            .unwrap();
        store.claim_approval("a1").unwrap();
        let claimed = |store: &Store| store.get_approval("a1").unwrap().unwrap().claimed_ms;
        assert!(claimed(&store).is_some());
        assert!(store.stale_approval_claims(60_000).unwrap().is_empty());
        assert_eq!(store.stale_approval_claims(-1).unwrap(), vec!["a1"]);
        store.release_approval("a1").unwrap();
        assert!(claimed(&store).is_none());

        store.claim_approval("a1").unwrap();
        assert!(store.decide_approval("a1", &allow()).unwrap());
        assert!(claimed(&store).is_none());
        store.claim_approval("a1").unwrap();
        assert!(claimed(&store).is_none());
    }
}
