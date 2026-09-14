//! The durable identity map between a tracon session and an OpenCode one.
//!
//! OpenCode names everything it makes — `ses_`, `msg_`, `prt_`, `per_`,
//! `que_`, `pty_` — and the node names everything *it* makes. Ingestion is the
//! seam, and a seam that lives only in a process's memory cannot survive the
//! thing it exists to survive: a dropped stream, a restarted node, an event
//! delivered twice. So the map is a table.
//!
//! Three facts are kept, and each one answers a question the node would
//! otherwise have to guess at:
//!
//! * **Which upstream session is this?** `opencode_session`, keyed by the
//!   harness's `ses_` id. A child session OpenCode makes for itself (a fork, a
//!   background subagent) is recorded here too, with its parent and
//!   `untracked=1`: there is no route to create one through tracon yet, so it
//!   is written down and surfaced rather than driven.
//! * **What has already been ingested?** `last_seq`, the durable aggregate
//!   sequence. It is the one number `?after=` is resumed from, and advancing it
//!   is the idempotence primitive: the advance is a conditional UPDATE, so a
//!   sequence that has already been ingested cannot be ingested again, whether
//!   it arrives from a reconnect overlap, a restart replay, or a snapshot.
//! * **What did an upstream id become?** `opencode_object`, so a permission
//!   re-raised after a reconnect is recognised as the same request rather than
//!   asked a second time. A tool call has no OpenCode id — its `callID` is
//!   provider text — and is keyed by `<assistantMessageID>/<callID>`.
//!
//! `opencode_intent` is the fourth: what a mediated mutation was about to do,
//! written before it is dispatched. A prompt whose POST times out may or may
//! not have been admitted, and the honest answer is neither "sent" nor "not
//! sent" but "ask". The row is what makes asking possible, and the session's
//! `uncertain` flag is what stops a second prompt from being sent in the
//! meantime — the same shape as a publication's `uncertain` state.

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::{now_ms, Result, Store};

/// What a mapped upstream object is. The spelling is the stored one.
pub mod object_kind {
    pub const MESSAGE: &str = "message";
    pub const PART: &str = "part";
    pub const PERMISSION: &str = "permission";
    pub const QUESTION: &str = "question";
    pub const PTY: &str = "pty";
    /// Keyed by `<assistantMessageID>/<callID>`: a tool call has no id of its
    /// own upstream.
    pub const TOOL_CALL: &str = "tool_call";
}

/// What a mediated mutation was about to do.
pub mod intent_kind {
    pub const PROMPT: &str = "prompt";
    pub const ABORT: &str = "abort";
    pub const PERMISSION_REPLY: &str = "permission_reply";
    /// Any other mediated call the gateway forwards — a revert, a compact, a
    /// model switch, a command. Its effect is observable in the session's own
    /// state rather than in a snapshot of its own, so reconciliation settles
    /// it without re-sending; the note the gateway wrote survives, because
    /// settling never overwrites a note with an empty one.
    pub const API: &str = "api";
}

/// How a mediated mutation ended.
pub mod intent_state {
    /// Sent, outcome not yet known.
    pub const DISPATCHED: &str = "dispatched";
    /// The harness was observed to have taken it.
    pub const ADMITTED: &str = "admitted";
    /// It provably did not land; nothing happened upstream.
    pub const FAILED: &str = "failed";
    /// The outcome cannot be determined without asking the harness.
    pub const UNCERTAIN: &str = "uncertain";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenCodeSessionRow {
    /// The harness's own `ses_` id.
    pub upstream_id: String,
    /// The tracon session it belongs to; `None` for an untracked child.
    pub session_id: Option<String>,
    pub parent_id: Option<String>,
    pub untracked: i64,
    pub last_seq: i64,
    pub uncertain: i64,
    pub uncertain_reason: Option<String>,
    /// The harness no longer has this session.
    pub gone: i64,
    pub created_ms: i64,
    pub updated_ms: i64,
}

impl OpenCodeSessionRow {
    fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            upstream_id: r.get("upstream_id")?,
            session_id: r.get("session_id")?,
            parent_id: r.get("parent_id")?,
            untracked: r.get("untracked")?,
            last_seq: r.get("last_seq")?,
            uncertain: r.get("uncertain")?,
            uncertain_reason: r.get("uncertain_reason")?,
            gone: r.get("gone")?,
            created_ms: r.get("created_ms")?,
            updated_ms: r.get("updated_ms")?,
        })
    }

    pub fn is_untracked(&self) -> bool {
        self.untracked != 0
    }

    pub fn is_uncertain(&self) -> bool {
        self.uncertain != 0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenCodeObjectRow {
    pub session_id: String,
    pub kind: String,
    pub upstream_id: String,
    /// The tracon id this became: a permission request id, a tool call id.
    pub ref_id: Option<String>,
    /// The tracon event it produced, when it produced one.
    pub event_seq: Option<i64>,
    /// The last durable sequence that touched it.
    pub seq: i64,
    pub state: Option<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

impl OpenCodeObjectRow {
    fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            session_id: r.get("session_id")?,
            kind: r.get("kind")?,
            upstream_id: r.get("upstream_id")?,
            ref_id: r.get("ref_id")?,
            event_seq: r.get("event_seq")?,
            seq: r.get("seq")?,
            state: r.get("state")?,
            created_ms: r.get("created_ms")?,
            updated_ms: r.get("updated_ms")?,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenCodeIntentRow {
    pub id: String,
    pub session_id: String,
    pub kind: String,
    pub target: Option<String>,
    pub detail: Option<String>,
    pub state: String,
    pub note: Option<String>,
    pub instance: String,
    pub created_ms: i64,
    pub updated_ms: i64,
}

impl OpenCodeIntentRow {
    fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: r.get("id")?,
            session_id: r.get("session_id")?,
            kind: r.get("kind")?,
            target: r.get("target")?,
            detail: r.get("detail")?,
            state: r.get("state")?,
            note: r.get("note")?,
            instance: r.get("instance")?,
            created_ms: r.get("created_ms")?,
            updated_ms: r.get("updated_ms")?,
        })
    }
}

impl Store {
    /// Tie a tracon session to the OpenCode session the handshake created.
    /// Idempotent: re-binding the same pair after a reconnect keeps the
    /// sequence and everything already mapped, because that is precisely what
    /// must not be lost.
    pub fn opencode_bind(
        &self,
        session_id: &str,
        upstream_id: &str,
        parent_id: Option<&str>,
    ) -> Result<OpenCodeSessionRow> {
        {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO opencode_session
                    (upstream_id, session_id, parent_id, untracked, last_seq, created_ms, updated_ms)
                 VALUES (?1,?2,?3,0,0,?4,?4)
                 ON CONFLICT(upstream_id) DO UPDATE SET
                    session_id=excluded.session_id,
                    parent_id=COALESCE(excluded.parent_id, opencode_session.parent_id),
                    untracked=0,
                    updated_ms=excluded.updated_ms",
                params![upstream_id, session_id, parent_id, now_ms()],
            )?;
        }
        self.opencode_session(upstream_id)?
            .ok_or_else(|| super::StoreError::Invalid("opencode binding vanished".into()))
    }

    pub fn opencode_session(&self, upstream_id: &str) -> Result<Option<OpenCodeSessionRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT * FROM opencode_session WHERE upstream_id=?1",
            [upstream_id],
            OpenCodeSessionRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn opencode_session_of(&self, session_id: &str) -> Result<Option<OpenCodeSessionRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT * FROM opencode_session WHERE session_id=?1",
            [session_id],
            OpenCodeSessionRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// A session OpenCode made for itself, named as a child of one the node
    /// owns. Recorded with its lineage and marked untracked; there is no route
    /// to create one through tracon, so it is never driven. Returns whether
    /// this is the first time it has been seen, so the operator is told once
    /// rather than on every replay.
    pub fn opencode_record_child(&self, upstream_id: &str, parent_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let inserted = conn.execute(
            "INSERT INTO opencode_session
                (upstream_id, session_id, parent_id, untracked, last_seq, created_ms, updated_ms)
             VALUES (?1, NULL, ?2, 1, 0, ?3, ?3)
             ON CONFLICT(upstream_id) DO NOTHING",
            params![upstream_id, parent_id, now_ms()],
        )?;
        Ok(inserted > 0)
    }

    /// Children of an upstream session, tracked or not.
    pub fn opencode_children(&self, parent_id: &str) -> Result<Vec<OpenCodeSessionRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT * FROM opencode_session WHERE parent_id=?1 ORDER BY created_ms")?;
        let rows = stmt
            .query_map([parent_id], OpenCodeSessionRow::from_row)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// The durable sequence this session has ingested up to. What `?after=` is
    /// resumed from, and the only reason a restart does not lose a turn.
    pub fn opencode_last_seq(&self, session_id: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT last_seq FROM opencode_session WHERE session_id=?1",
                [session_id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    /// Claim one durable sequence for ingestion. True exactly once per
    /// sequence: the comparison is inside the UPDATE, so two deliveries of the
    /// same event — a reconnect overlap, a snapshot and a stream racing — can
    /// never both be admitted, and the caller that loses knows to drop it.
    ///
    /// A sequence lower than the one already recorded is refused too: the
    /// durable stream is ordered, so an earlier sequence arriving later is a
    /// replay by definition.
    pub fn opencode_admit_seq(&self, session_id: &str, seq: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "UPDATE opencode_session SET last_seq=?2, updated_ms=?3
              WHERE session_id=?1 AND last_seq < ?2",
            params![session_id, seq, now_ms()],
        )? > 0)
    }

    /// Map an upstream id to what it became here. Idempotent on the key, and
    /// never overwrites a mapping with an empty one: a later event that only
    /// carries the id keeps whatever the first one recorded.
    pub fn opencode_map(
        &self,
        session_id: &str,
        kind: &str,
        upstream_id: &str,
        ref_id: Option<&str>,
        event_seq: Option<i64>,
        seq: i64,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let before: i64 = conn.query_row(
            "SELECT COUNT(*) FROM opencode_object
              WHERE session_id=?1 AND kind=?2 AND upstream_id=?3",
            params![session_id, kind, upstream_id],
            |r| r.get(0),
        )?;
        conn.execute(
            "INSERT INTO opencode_object
                (session_id, kind, upstream_id, ref_id, event_seq, seq, created_ms, updated_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?7)
             ON CONFLICT(session_id, kind, upstream_id) DO UPDATE SET
                ref_id=COALESCE(excluded.ref_id, opencode_object.ref_id),
                event_seq=COALESCE(excluded.event_seq, opencode_object.event_seq),
                seq=MAX(excluded.seq, opencode_object.seq),
                updated_ms=excluded.updated_ms",
            params![
                session_id,
                kind,
                upstream_id,
                ref_id,
                event_seq,
                seq,
                now_ms()
            ],
        )?;
        Ok(before == 0)
    }

    pub fn opencode_object(
        &self,
        session_id: &str,
        kind: &str,
        upstream_id: &str,
    ) -> Result<Option<OpenCodeObjectRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT * FROM opencode_object WHERE session_id=?1 AND kind=?2 AND upstream_id=?3",
            params![session_id, kind, upstream_id],
            OpenCodeObjectRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Which tracon session an upstream object belongs to, asked without
    /// naming one. The gateway's mount is a tracon session, but OpenCode's own
    /// v1 permission route (`POST /permission/{id}/reply`) names no session at
    /// all, so the only way to see that a reply is aimed at another session's
    /// request is to ask what this id was mapped to when it was raised.
    pub fn opencode_object_owner(&self, kind: &str, upstream_id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT session_id FROM opencode_object WHERE kind=?1 AND upstream_id=?2",
            params![kind, upstream_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn opencode_objects(&self, session_id: &str, kind: &str) -> Result<Vec<OpenCodeObjectRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM opencode_object WHERE session_id=?1 AND kind=?2 ORDER BY created_ms",
        )?;
        let rows = stmt
            .query_map(params![session_id, kind], OpenCodeObjectRow::from_row)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// What an upstream object is now: a permission `pending`, `answered`, or
    /// whatever the ingestion layer last observed.
    pub fn opencode_set_object_state(
        &self,
        session_id: &str,
        kind: &str,
        upstream_id: &str,
        state: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE opencode_object SET state=?4, updated_ms=?5
              WHERE session_id=?1 AND kind=?2 AND upstream_id=?3",
            params![session_id, kind, upstream_id, state, now_ms()],
        )?;
        Ok(())
    }

    /// Every permission request this session has raised, open or resolved.
    /// Reconciliation needs the resolved ones too: a request still pending
    /// upstream that this node already answered has to be answered again, and
    /// the answer is on the row.
    pub fn permissions_for_session(&self, session_id: &str) -> Result<Vec<super::PermissionRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT * FROM permission_request WHERE session_id=?1 ORDER BY created_ms")?;
        let rows = stmt
            .query_map([session_id], super::PermissionRow::from_row)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// The outcome of a mediated mutation cannot be determined. Dependent
    /// operations are refused until reconciliation settles it or the operator
    /// clears it.
    pub fn opencode_set_uncertain(&self, session_id: &str, reason: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE opencode_session SET uncertain=1, uncertain_reason=?2, updated_ms=?3
              WHERE session_id=?1",
            params![session_id, reason, now_ms()],
        )?;
        Ok(())
    }

    /// Reconciliation established what actually happened.
    pub fn opencode_clear_uncertain(&self, session_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "UPDATE opencode_session SET uncertain=0, uncertain_reason=NULL, updated_ms=?2
              WHERE session_id=?1 AND uncertain=1",
            params![session_id, now_ms()],
        )? > 0)
    }

    /// The harness no longer has this session. Recorded on the mapping as well
    /// as on the session row, so the reason survives an archived session.
    pub fn opencode_set_gone(&self, session_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE opencode_session SET gone=1, updated_ms=?2 WHERE session_id=?1",
            params![session_id, now_ms()],
        )?;
        Ok(())
    }

    /// Write down what a mediated mutation is about to do, before it is
    /// dispatched. The ordering is the whole point: a crash or a timeout
    /// between this row and the answer leaves something that says "this may
    /// have happened", which is the truth.
    pub fn opencode_intent_begin(
        &self,
        id: &str,
        session_id: &str,
        kind: &str,
        target: Option<&str>,
        detail: Option<&str>,
    ) -> Result<OpenCodeIntentRow> {
        {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO opencode_intent
                    (id, session_id, kind, target, detail, state, instance, created_ms, updated_ms)
                 VALUES (?1,?2,?3,?4,?5,'dispatched',?6,?7,?7)
                 ON CONFLICT(id) DO UPDATE SET
                    state='dispatched', instance=excluded.instance, updated_ms=excluded.updated_ms",
                params![
                    id,
                    session_id,
                    kind,
                    target,
                    detail,
                    crate::process::instance_id(),
                    now_ms()
                ],
            )?;
        }
        self.opencode_intent(id)?
            .ok_or_else(|| super::StoreError::Invalid("intent record vanished".into()))
    }

    pub fn opencode_intent(&self, id: &str) -> Result<Option<OpenCodeIntentRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT * FROM opencode_intent WHERE id=?1",
            [id],
            OpenCodeIntentRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn opencode_intent_settle(&self, id: &str, state: &str, note: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE opencode_intent SET state=?2, note=COALESCE(?3, note), updated_ms=?4
              WHERE id=?1",
            params![id, state, note, now_ms()],
        )?;
        Ok(())
    }

    /// Intents whose outcome is not settled, oldest first: what
    /// reconciliation has to ask the harness about.
    pub fn opencode_unsettled_intents(&self, session_id: &str) -> Result<Vec<OpenCodeIntentRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM opencode_intent
              WHERE session_id=?1 AND state IN ('dispatched','uncertain')
              ORDER BY created_ms",
        )?;
        let rows = stmt
            .query_map([session_id], OpenCodeIntentRow::from_row)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// An intent this process did not dispatch and that is still `dispatched`
    /// was left in flight by a process that is gone: nobody is waiting for its
    /// answer, and nobody knows what it was. That is `uncertain`, not failed —
    /// the same reading a publication gets, for the same reason.
    ///
    /// Only safe at startup, when no mutation of this process's is in flight.
    pub fn opencode_reconcile_interrupted(&self, instance: &str) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let intents = conn.execute(
            "UPDATE opencode_intent
                SET state='uncertain',
                    note=COALESCE(note, 'the node restarted before this mutation reported its outcome'),
                    updated_ms=?2
              WHERE state='dispatched' AND instance <> ?1",
            params![instance, now_ms()],
        )?;
        conn.execute(
            "UPDATE opencode_session
                SET uncertain=1,
                    uncertain_reason=COALESCE(uncertain_reason,
                        'the node restarted with a mediated mutation in flight'),
                    updated_ms=?2
              WHERE session_id IN (
                  SELECT session_id FROM opencode_intent
                   WHERE state='uncertain' AND instance <> ?1)",
            params![instance, now_ms()],
        )?;
        Ok(intents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{NodeRow, SessionRow};

    fn store_with_session(id: &str) -> Store {
        let store = Store::open_in_memory().unwrap();
        store
            .put_node(&NodeRow {
                id: "n1".into(),
                name: "t".into(),
                state: "ready".into(),
                failed_check: None,
                failed_detail: None,
                harness_id: "opencode".into(),
                harness_pinned: "1.18.30".into(),
                harness_found: None,
                models_json: None,
                checked_at_ms: None,
                is_self: 1,
                x25519_pub: None,
                last_seen_ms: None,
                reachable: 1,
                providers_json: None,
                app_version: None,
                wire_contract: None,
                policy_identity: None,
                policy_sha256: None,
                policy_receipt_v1: None,
            })
            .unwrap();
        let mut row = SessionRow {
            id: id.into(),
            node_id: "n1".into(),
            channel: "personal".into(),
            work_item_id: None,
            repo_path: "/r".into(),
            worktree_path: None,
            branch: "b".into(),
            harness_id: "opencode".into(),
            harness_version: "1.18.30".into(),
            harness_agent: None,
            harness_found: None,
            harness_protocol: None,
            harness_session_id: None,
            container_name: None,
            model: "m".into(),
            project_id: None,
            phase: "execute".into(),
            policy_version: None,
            review_id: None,
            budget_tokens: 0,
            tokens_used: 0,
            cost_usd: None,
            context_used: None,
            context_size: None,
            state: "running".into(),
            end_reason: None,
            last_error: None,
            turn_active: 0,
            draft: None,
            draft_updated_ms: None,
            created_ms: now_ms(),
            started_mono_ms: None,
            ended_mono_ms: None,
            updated_ms: now_ms(),
            archived_ms: None,
            legacy_ms: None,
            parent_session: None,
            continued_from: None,
            manifest_digest: None,
        };
        row.id = id.into();
        store.insert_session(&row).unwrap();
        store
    }

    #[test]
    fn a_sequence_is_admitted_exactly_once() {
        let store = store_with_session("s1");
        store.opencode_bind("s1", "ses_a", None).unwrap();
        assert!(store.opencode_admit_seq("s1", 1).unwrap());
        assert!(!store.opencode_admit_seq("s1", 1).unwrap(), "a re-delivery");
        assert!(store.opencode_admit_seq("s1", 2).unwrap());
        assert!(
            !store.opencode_admit_seq("s1", 1).unwrap(),
            "an earlier sequence arriving later is a replay by definition"
        );
        assert_eq!(store.opencode_last_seq("s1").unwrap(), 2);
    }

    #[test]
    fn rebinding_keeps_the_sequence_and_the_map() {
        let store = store_with_session("s1");
        store.opencode_bind("s1", "ses_a", None).unwrap();
        store.opencode_admit_seq("s1", 7).unwrap();
        store
            .opencode_map("s1", object_kind::PERMISSION, "per_1", Some("p1"), None, 7)
            .unwrap();
        let again = store.opencode_bind("s1", "ses_a", None).unwrap();
        assert_eq!(
            again.last_seq, 7,
            "a reconnect must not rewind what was ingested"
        );
        let mapped = store
            .opencode_object("s1", object_kind::PERMISSION, "per_1")
            .unwrap()
            .unwrap();
        assert_eq!(mapped.ref_id.as_deref(), Some("p1"));
    }

    #[test]
    fn a_mapping_is_new_only_the_first_time_and_never_loses_what_it_holds() {
        let store = store_with_session("s1");
        store.opencode_bind("s1", "ses_a", None).unwrap();
        assert!(store
            .opencode_map("s1", object_kind::MESSAGE, "msg_1", Some("e1"), Some(4), 2)
            .unwrap());
        assert!(!store
            .opencode_map("s1", object_kind::MESSAGE, "msg_1", None, None, 5)
            .unwrap());
        let row = store
            .opencode_object("s1", object_kind::MESSAGE, "msg_1")
            .unwrap()
            .unwrap();
        assert_eq!(row.ref_id.as_deref(), Some("e1"));
        assert_eq!(row.event_seq, Some(4));
        assert_eq!(row.seq, 5, "the latest sequence that touched it");
    }

    /// The gateway asks this about a permission id that names no session, so
    /// an unmapped id has to answer "nobody knows" rather than "mine".
    #[test]
    fn an_upstream_object_says_which_session_it_belongs_to() {
        let store = store_with_session("s1");
        store.opencode_bind("s1", "ses_a", None).unwrap();
        store
            .opencode_map("s1", object_kind::PERMISSION, "per_1", Some("p1"), None, 1)
            .unwrap();
        assert_eq!(
            store
                .opencode_object_owner(object_kind::PERMISSION, "per_1")
                .unwrap()
                .as_deref(),
            Some("s1")
        );
        assert_eq!(
            store
                .opencode_object_owner(object_kind::PERMISSION, "per_unknown")
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .opencode_object_owner(object_kind::MESSAGE, "per_1")
                .unwrap(),
            None,
            "the kind is part of the key"
        );
    }

    #[test]
    fn a_child_session_is_recorded_once_with_its_lineage() {
        let store = store_with_session("s1");
        store.opencode_bind("s1", "ses_parent", None).unwrap();
        assert!(store
            .opencode_record_child("ses_child", "ses_parent")
            .unwrap());
        assert!(
            !store
                .opencode_record_child("ses_child", "ses_parent")
                .unwrap(),
            "the operator is told once, not on every replay"
        );
        let children = store.opencode_children("ses_parent").unwrap();
        assert_eq!(children.len(), 1);
        assert!(children[0].is_untracked());
        assert_eq!(children[0].session_id, None);
    }

    #[test]
    fn an_intent_a_dead_process_left_in_flight_is_uncertain_not_failed() {
        let store = store_with_session("s1");
        store.opencode_bind("s1", "ses_a", None).unwrap();
        store
            .opencode_intent_begin("i1", "s1", intent_kind::PROMPT, None, Some("digest"))
            .unwrap();
        assert_eq!(store.opencode_reconcile_interrupted("another").unwrap(), 1);
        let intent = store.opencode_intent("i1").unwrap().unwrap();
        assert_eq!(intent.state, intent_state::UNCERTAIN);
        assert!(intent.note.unwrap().contains("restarted"));
        assert!(store
            .opencode_session_of("s1")
            .unwrap()
            .unwrap()
            .is_uncertain());
        // This process's own in-flight mutation is left alone.
        store
            .opencode_intent_begin("i2", "s1", intent_kind::PROMPT, None, None)
            .unwrap();
        assert_eq!(
            store
                .opencode_reconcile_interrupted(crate::process::instance_id())
                .unwrap(),
            0
        );
        assert_eq!(
            store.opencode_intent("i2").unwrap().unwrap().state,
            intent_state::DISPATCHED
        );
    }

    #[test]
    fn uncertainty_is_set_and_cleared_on_the_session_it_belongs_to() {
        let store = store_with_session("s1");
        store.opencode_bind("s1", "ses_a", None).unwrap();
        store
            .opencode_set_uncertain("s1", "the prompt timed out")
            .unwrap();
        let row = store.opencode_session_of("s1").unwrap().unwrap();
        assert!(row.is_uncertain());
        assert_eq!(
            row.uncertain_reason.as_deref(),
            Some("the prompt timed out")
        );
        assert!(store.opencode_clear_uncertain("s1").unwrap());
        assert!(!store.opencode_clear_uncertain("s1").unwrap());
        assert!(!store
            .opencode_session_of("s1")
            .unwrap()
            .unwrap()
            .is_uncertain());
    }
}
