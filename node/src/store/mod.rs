//! The node's local SQLite store. Single writer, accessed through a mutex.
//!
//! Calls run synchronously on the async runtime. The database is local, indexed,
//! and single-operator, so queries are short; the two largest reads (a review
//! diff, the event replay) are bounded — the replay is paged in batches by the
//! stream. If a workload ever makes a query long enough to stall a runtime
//! worker, move that call behind `spawn_blocking`; today none is.

mod schema;

use std::{
    path::Path,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use records::*;

pub struct Store {
    conn: Mutex<Connection>,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

type Result<T> = std::result::Result<T, StoreError>;

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let conn = Connection::open(path)?;
        schema::migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        schema::migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    // ---- node ----

    pub fn put_node(&self, n: &NodeRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO node (id, name, state, failed_check, failed_detail, harness_id,
                harness_pinned, harness_found, models_json, checked_at_ms, is_self, x25519_pub,
                last_seen_ms, reachable)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
             ON CONFLICT(id) DO UPDATE SET name=?2, state=?3, failed_check=?4, failed_detail=?5,
                harness_id=?6, harness_pinned=?7, harness_found=?8, models_json=?9, checked_at_ms=?10,
                is_self=?11, x25519_pub=?12, last_seen_ms=?13, reachable=?14",
            rusqlite::params![
                n.id, n.name, n.state, n.failed_check, n.failed_detail, n.harness_id,
                n.harness_pinned, n.harness_found, n.models_json, n.checked_at_ms, n.is_self,
                n.x25519_pub, n.last_seen_ms, n.reachable
            ],
        )?;
        Ok(())
    }

    /// The row for this node, if it has run before. Peers live in the same
    /// table, so the self row is the one flagged, never "the first".
    pub fn self_node_id(&self) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT id FROM node WHERE is_self=1 LIMIT 1", [], |r| {
            r.get(0)
        })
        .optional()
        .map_err(Into::into)
    }

    pub fn list_nodes(&self) -> Result<Vec<NodeRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM node ORDER BY is_self DESC, name ASC")?;
        let rows = stmt
            .query_map([], NodeRow::from_row)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// Move this node's identity from `old` to `new` across every table that
    /// names it, in one transaction. The pre-identity node id was a uuid; the
    /// mesh id is the Ed25519 key. Also claims permission rows written with an
    /// empty node id by an earlier build.
    pub fn rekey_self_node(&self, old: &str, new: &str) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute_batch("PRAGMA defer_foreign_keys = ON")?;
        tx.execute("UPDATE node SET id=?2 WHERE id=?1", [old, new])?;
        tx.execute("UPDATE session SET node_id=?2 WHERE node_id=?1", [old, new])?;
        tx.execute(
            "UPDATE permission_request SET node_id=?2 WHERE node_id=?1 OR node_id=''",
            [old, new],
        )?;
        tx.execute("UPDATE review SET node_id=?2 WHERE node_id=?1", [old, new])?;
        tx.execute("UPDATE event SET node_id=?2 WHERE node_id=?1", [old, new])?;
        tx.commit()?;
        Ok(())
    }

    pub fn get_node(&self, id: &str) -> Result<Option<NodeRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT * FROM node WHERE id=?1", [id], NodeRow::from_row)
            .optional()
            .map_err(Into::into)
    }

    // ---- session ----

    pub fn insert_session(&self, s: &SessionRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO session (id, node_id, channel, work_item_id, repo_path, worktree_path,
                branch, harness_id, harness_version, harness_session_id, container_name, model,
                budget_tokens, tokens_used, cost_usd, context_used, context_size, state, end_reason,
                last_error, turn_active, draft, draft_updated_ms, created_ms, started_mono_ms,
                ended_mono_ms, updated_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,
                ?23,?24,?25,?26,?27)",
            rusqlite::params![
                s.id,
                s.node_id,
                s.channel,
                s.work_item_id,
                s.repo_path,
                s.worktree_path,
                s.branch,
                s.harness_id,
                s.harness_version,
                s.harness_session_id,
                s.container_name,
                s.model,
                s.budget_tokens,
                s.tokens_used,
                s.cost_usd,
                s.context_used,
                s.context_size,
                s.state,
                s.end_reason,
                s.last_error,
                s.turn_active,
                s.draft,
                s.draft_updated_ms,
                s.created_ms,
                s.started_mono_ms,
                s.ended_mono_ms,
                s.updated_ms
            ],
        )?;
        Ok(())
    }

    pub fn get_session(&self, id: &str) -> Result<Option<SessionRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT * FROM session WHERE id=?1",
            [id],
            SessionRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn list_sessions(&self, state: Option<&str>) -> Result<Vec<SessionRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(match state {
            Some(_) => "SELECT * FROM session WHERE state=?1 ORDER BY created_ms DESC",
            None => "SELECT * FROM session ORDER BY created_ms DESC",
        })?;
        let rows = match state {
            Some(s) => stmt
                .query_map([s], SessionRow::from_row)?
                .collect::<std::result::Result<_, _>>()?,
            None => stmt
                .query_map([], SessionRow::from_row)?
                .collect::<std::result::Result<_, _>>()?,
        };
        Ok(rows)
    }

    /// Applies a set of column updates to one session and stamps `updated_ms`.
    pub fn update_session(&self, id: &str, patch: SessionPatch) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        patch.apply(&conn, id)
    }

    pub fn set_draft(&self, id: &str, text: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE session SET draft=?2, draft_updated_ms=?3, updated_ms=?3 WHERE id=?1",
            rusqlite::params![id, text, now_ms()],
        )?;
        Ok(())
    }

    // ---- event ----

    /// Appends an event and returns its assigned `seq` (also the SSE id).
    pub fn append_event(&self, e: &NewEvent) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        // The owning node is the session's; derived here so no call site has
        // to carry it.
        conn.execute(
            "INSERT INTO event (session_id, work_item_id, kind, ref_id, payload, at_ms, mono_ms, node_id)
             VALUES (?1,?2,?3,?4,?5,?6,?7, (SELECT node_id FROM session WHERE id=?1))",
            rusqlite::params![
                e.session_id,
                e.work_item_id,
                e.kind,
                e.ref_id,
                serde_json::to_string(&e.payload)?,
                e.at_ms,
                e.mono_ms
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn events_after(
        &self,
        session_id: &str,
        after_seq: i64,
        limit: i64,
    ) -> Result<Vec<EventRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT seq, node_id, session_id, work_item_id, kind, ref_id, payload, at_ms, mono_ms
             FROM event WHERE session_id=?1 AND seq>?2 ORDER BY seq LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(
                rusqlite::params![session_id, after_seq, limit],
                EventRow::from_row,
            )?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// Every persisted event across all sessions after `after_seq` (the stream
    /// replay path for `Last-Event-ID`).
    pub fn all_events_after(&self, after_seq: i64, limit: i64) -> Result<Vec<EventRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT seq, node_id, session_id, work_item_id, kind, ref_id, payload, at_ms, mono_ms
             FROM event WHERE seq>?1 ORDER BY seq LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![after_seq, limit], EventRow::from_row)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    // ---- permission_request ----

    pub fn insert_permission(&self, p: &PermissionRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO permission_request (id, session_id, node_id, rpc_id, tool_call_id, title,
                kind, raw_input, options, state, answer_option_id, created_ms, created_mono_ms,
                resolved_mono_ms, expires_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            rusqlite::params![
                p.id,
                p.session_id,
                p.node_id,
                p.rpc_id,
                p.tool_call_id,
                p.title,
                p.kind,
                p.raw_input,
                p.options,
                p.state,
                p.answer_option_id,
                p.created_ms,
                p.created_mono_ms,
                p.resolved_mono_ms,
                p.expires_ms
            ],
        )?;
        Ok(())
    }

    pub fn resolve_permission(
        &self,
        id: &str,
        state: &str,
        option_id: Option<&str>,
        resolved_mono_ms: i64,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE permission_request SET state=?2, answer_option_id=?3, resolved_mono_ms=?4
             WHERE id=?1 AND state='new'",
            rusqlite::params![id, state, option_id, resolved_mono_ms],
        )?;
        Ok(n == 1)
    }

    pub fn get_permission(&self, id: &str) -> Result<Option<PermissionRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT * FROM permission_request WHERE id=?1",
            [id],
            PermissionRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// The waiting bay, ordered as DESIGN.md decided: permission requests before
    /// review approvals (reviews do not exist yet), then oldest first.
    pub fn open_permissions(&self) -> Result<Vec<PermissionRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM permission_request WHERE state='new' ORDER BY created_ms ASC",
        )?;
        let rows = stmt
            .query_map([], PermissionRow::from_row)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }
}

mod records {
    use super::*;

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct NodeRow {
        pub id: String,
        pub name: String,
        pub state: String,
        pub failed_check: Option<String>,
        pub failed_detail: Option<String>,
        pub harness_id: String,
        pub harness_pinned: String,
        pub harness_found: Option<String>,
        pub models_json: Option<String>,
        pub checked_at_ms: Option<i64>,
        /// 1 for the row that is this node; peers are 0.
        #[serde(default)]
        pub is_self: i64,
        #[serde(default)]
        pub x25519_pub: Option<String>,
        #[serde(default)]
        pub last_seen_ms: Option<i64>,
        /// 0 once the hub has not heard from a peer within the presence window.
        #[serde(default = "one")]
        pub reachable: i64,
    }

    fn one() -> i64 {
        1
    }

    impl NodeRow {
        pub(super) fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Self> {
            Ok(Self {
                id: r.get("id")?,
                name: r.get("name")?,
                state: r.get("state")?,
                failed_check: r.get("failed_check")?,
                failed_detail: r.get("failed_detail")?,
                harness_id: r.get("harness_id")?,
                harness_pinned: r.get("harness_pinned")?,
                harness_found: r.get("harness_found")?,
                models_json: r.get("models_json")?,
                checked_at_ms: r.get("checked_at_ms")?,
                is_self: r.get("is_self")?,
                x25519_pub: r.get("x25519_pub")?,
                last_seen_ms: r.get("last_seen_ms")?,
                reachable: r.get("reachable")?,
            })
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct SessionRow {
        pub id: String,
        pub node_id: String,
        pub channel: String,
        pub work_item_id: Option<String>,
        pub repo_path: String,
        pub worktree_path: Option<String>,
        pub branch: String,
        pub harness_id: String,
        pub harness_version: String,
        pub harness_session_id: Option<String>,
        pub container_name: Option<String>,
        pub model: String,
        pub budget_tokens: i64,
        pub tokens_used: i64,
        pub cost_usd: Option<f64>,
        pub context_used: Option<i64>,
        pub context_size: Option<i64>,
        pub state: String,
        pub end_reason: Option<String>,
        pub last_error: Option<String>,
        pub turn_active: i64,
        pub draft: Option<String>,
        pub draft_updated_ms: Option<i64>,
        pub created_ms: i64,
        pub started_mono_ms: Option<i64>,
        pub ended_mono_ms: Option<i64>,
        pub updated_ms: i64,
    }

    impl SessionRow {
        pub(super) fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Self> {
            Ok(Self {
                id: r.get("id")?,
                node_id: r.get("node_id")?,
                channel: r.get("channel")?,
                work_item_id: r.get("work_item_id")?,
                repo_path: r.get("repo_path")?,
                worktree_path: r.get("worktree_path")?,
                branch: r.get("branch")?,
                harness_id: r.get("harness_id")?,
                harness_version: r.get("harness_version")?,
                harness_session_id: r.get("harness_session_id")?,
                container_name: r.get("container_name")?,
                model: r.get("model")?,
                budget_tokens: r.get("budget_tokens")?,
                tokens_used: r.get("tokens_used")?,
                cost_usd: r.get("cost_usd")?,
                context_used: r.get("context_used")?,
                context_size: r.get("context_size")?,
                state: r.get("state")?,
                end_reason: r.get("end_reason")?,
                last_error: r.get("last_error")?,
                turn_active: r.get("turn_active")?,
                draft: r.get("draft")?,
                draft_updated_ms: r.get("draft_updated_ms")?,
                created_ms: r.get("created_ms")?,
                started_mono_ms: r.get("started_mono_ms")?,
                ended_mono_ms: r.get("ended_mono_ms")?,
                updated_ms: r.get("updated_ms")?,
            })
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct EventRow {
        pub seq: i64,
        /// The owning node. Empty only for rows older than the column.
        #[serde(default)]
        pub node_id: String,
        pub session_id: String,
        pub work_item_id: Option<String>,
        pub kind: String,
        pub ref_id: Option<String>,
        pub payload: Value,
        pub at_ms: i64,
        pub mono_ms: i64,
    }

    impl EventRow {
        pub(super) fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Self> {
            let payload: String = r.get("payload")?;
            Ok(Self {
                seq: r.get("seq")?,
                node_id: r.get::<_, Option<String>>("node_id")?.unwrap_or_default(),
                session_id: r.get("session_id")?,
                work_item_id: r.get("work_item_id")?,
                kind: r.get("kind")?,
                ref_id: r.get("ref_id")?,
                payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
                at_ms: r.get("at_ms")?,
                mono_ms: r.get("mono_ms")?,
            })
        }
    }

    #[derive(Debug, Clone)]
    pub struct NewEvent {
        pub session_id: String,
        pub work_item_id: Option<String>,
        pub kind: String,
        pub ref_id: Option<String>,
        pub payload: Value,
        pub at_ms: i64,
        pub mono_ms: i64,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct PermissionRow {
        pub id: String,
        pub session_id: String,
        pub node_id: String,
        pub rpc_id: i64,
        pub tool_call_id: Option<String>,
        pub title: String,
        pub kind: Option<String>,
        pub raw_input: Option<String>,
        pub options: String,
        pub state: String,
        pub answer_option_id: Option<String>,
        pub created_ms: i64,
        pub created_mono_ms: i64,
        pub resolved_mono_ms: Option<i64>,
        pub expires_ms: i64,
    }

    impl PermissionRow {
        pub(super) fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Self> {
            Ok(Self {
                id: r.get("id")?,
                session_id: r.get("session_id")?,
                node_id: r.get("node_id")?,
                rpc_id: r.get("rpc_id")?,
                tool_call_id: r.get("tool_call_id")?,
                title: r.get("title")?,
                kind: r.get("kind")?,
                raw_input: r.get("raw_input")?,
                options: r.get("options")?,
                state: r.get("state")?,
                answer_option_id: r.get("answer_option_id")?,
                created_ms: r.get("created_ms")?,
                created_mono_ms: r.get("created_mono_ms")?,
                resolved_mono_ms: r.get("resolved_mono_ms")?,
                expires_ms: r.get("expires_ms")?,
            })
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ReviewRow {
        pub id: String,
        pub session_id: String,
        pub node_id: String,
        pub channel: String,
        pub kind: String,
        pub title: String,
        pub body: String,
        pub edited_title: Option<String>,
        pub edited_body: Option<String>,
        pub provider: String,
        pub target: String,
        pub diff: String,
        pub files: String,
        pub head_sha: String,
        pub base_ref: String,
        pub added: i64,
        pub removed: i64,
        pub state: String,
        pub verdict_reason: Option<String>,
        pub publish_result: Option<String>,
        pub claimed_ms: Option<i64>,
        pub created_ms: i64,
        pub created_mono_ms: i64,
        pub resolved_mono_ms: Option<i64>,
        pub updated_ms: i64,
    }

    impl ReviewRow {
        /// The text a verdict approves: what the operator edited if they did,
        /// otherwise what the agent wrote.
        pub fn approved_title(&self) -> &str {
            self.edited_title.as_deref().unwrap_or(&self.title)
        }
        pub fn approved_body(&self) -> &str {
            self.edited_body.as_deref().unwrap_or(&self.body)
        }

        pub(super) fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Self> {
            Ok(Self {
                id: r.get("id")?,
                session_id: r.get("session_id")?,
                node_id: r.get("node_id")?,
                channel: r.get("channel")?,
                kind: r.get("kind")?,
                title: r.get("title")?,
                body: r.get("body")?,
                edited_title: r.get("edited_title")?,
                edited_body: r.get("edited_body")?,
                provider: r.get("provider")?,
                target: r.get("target")?,
                diff: r.get("diff")?,
                files: r.get("files")?,
                head_sha: r.get("head_sha")?,
                base_ref: r.get("base_ref")?,
                added: r.get("added")?,
                removed: r.get("removed")?,
                state: r.get("state")?,
                verdict_reason: r.get("verdict_reason")?,
                publish_result: r.get("publish_result")?,
                claimed_ms: r.get("claimed_ms")?,
                created_ms: r.get("created_ms")?,
                created_mono_ms: r.get("created_mono_ms")?,
                resolved_mono_ms: r.get("resolved_mono_ms")?,
                updated_ms: r.get("updated_ms")?,
            })
        }
    }

    /// Sparse column updates for a session. Only `Some` fields are written.
    #[derive(Debug, Default)]
    pub struct SessionPatch {
        pub state: Option<String>,
        pub end_reason: Option<String>,
        pub last_error: Option<String>,
        pub worktree_path: Option<String>,
        pub harness_session_id: Option<String>,
        pub container_name: Option<String>,
        pub turn_active: Option<bool>,
        pub tokens_used: Option<i64>,
        pub cost_usd: Option<f64>,
        pub context_used: Option<i64>,
        pub context_size: Option<i64>,
        pub started_mono_ms: Option<i64>,
        pub ended_mono_ms: Option<i64>,
    }

    impl SessionPatch {
        pub fn state(s: impl Into<String>) -> Self {
            Self {
                state: Some(s.into()),
                ..Default::default()
            }
        }

        pub(super) fn apply(self, conn: &Connection, id: &str) -> Result<()> {
            let mut sets: Vec<&str> = Vec::new();
            let mut vals: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            macro_rules! push {
                ($col:literal, $field:expr) => {
                    if let Some(v) = $field {
                        sets.push(concat!($col, "=?"));
                        vals.push(Box::new(v));
                    }
                };
            }
            push!("state", self.state);
            push!("end_reason", self.end_reason);
            push!("last_error", self.last_error);
            push!("worktree_path", self.worktree_path);
            push!("harness_session_id", self.harness_session_id);
            push!("container_name", self.container_name);
            push!("turn_active", self.turn_active.map(|b| b as i64));
            push!("tokens_used", self.tokens_used);
            push!("cost_usd", self.cost_usd);
            push!("context_used", self.context_used);
            push!("context_size", self.context_size);
            push!("started_mono_ms", self.started_mono_ms);
            push!("ended_mono_ms", self.ended_mono_ms);
            if sets.is_empty() {
                return Ok(());
            }
            sets.push("updated_ms=?");
            vals.push(Box::new(now_ms()));
            let sql = format!("UPDATE session SET {} WHERE id=?", sets.join(", "));
            let mut params: Vec<&dyn rusqlite::ToSql> = vals.iter().map(|b| b.as_ref()).collect();
            let id_owned = id.to_string();
            params.push(&id_owned);
            conn.execute(&sql, params.as_slice())?;
            Ok(())
        }
    }
}

impl Store {
    // ---- review ----

    pub fn insert_review(&self, r: &ReviewRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO review (id, session_id, node_id, channel, kind, title, body, edited_title,
                edited_body, provider, target, diff, files, head_sha, base_ref, added, removed,
                state, verdict_reason, publish_result, claimed_ms, created_ms, created_mono_ms,
                resolved_mono_ms, updated_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,
                ?22,?23,?24,?25)",
            rusqlite::params![
                r.id,
                r.session_id,
                r.node_id,
                r.channel,
                r.kind,
                r.title,
                r.body,
                r.edited_title,
                r.edited_body,
                r.provider,
                r.target,
                r.diff,
                r.files,
                r.head_sha,
                r.base_ref,
                r.added,
                r.removed,
                r.state,
                r.verdict_reason,
                r.publish_result,
                r.claimed_ms,
                r.created_ms,
                r.created_mono_ms,
                r.resolved_mono_ms,
                r.updated_ms
            ],
        )?;
        Ok(())
    }

    pub fn get_review(&self, id: &str) -> Result<Option<ReviewRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT * FROM review WHERE id=?1",
            [id],
            ReviewRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Reviews still waiting on the operator, oldest first. Ordered after
    /// permission requests in the queue: requests expire, reviews do not.
    pub fn open_reviews(&self) -> Result<Vec<ReviewRow>> {
        let conn = self.conn.lock().unwrap();
        // `publishing` is a transient in-flight state and is deliberately left
        // out: a review mid-publish is not a card the operator can act on.
        let mut stmt = conn.prepare(
            "SELECT * FROM review WHERE state IN ('new','claimed','revising') \
             ORDER BY created_ms ASC",
        )?;
        let rows = stmt
            .query_map([], ReviewRow::from_row)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// Claim on open, as the design decided: a metric, not a lock.
    pub fn claim_review(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE review SET state='claimed', claimed_ms=COALESCE(claimed_ms, ?2), updated_ms=?2
             WHERE id=?1 AND state='new'",
            rusqlite::params![id, now_ms()],
        )?;
        Ok(())
    }

    /// Resolve a review exactly once. Returns false if it was already decided,
    /// so a second verdict cannot overwrite the first. Used for rejection; an
    /// approval goes through `begin_publish` → `finish_publish`.
    #[allow(clippy::too_many_arguments)]
    pub fn resolve_review(
        &self,
        id: &str,
        state: &str,
        reason: Option<&str>,
        edited_title: Option<&str>,
        edited_body: Option<&str>,
        publish_result: Option<&str>,
        resolved_mono_ms: i64,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE review SET state=?2, verdict_reason=?3, edited_title=COALESCE(?4, edited_title),
                edited_body=COALESCE(?5, edited_body), publish_result=?6, resolved_mono_ms=?7,
                updated_ms=?8
             WHERE id=?1 AND state IN ('new','claimed','revising')",
            rusqlite::params![
                id, state, reason, edited_title, edited_body, publish_result, resolved_mono_ms,
                now_ms()
            ],
        )?;
        Ok(n == 1)
    }

    /// Claim the publish. Moves a review awaiting a verdict into `publishing` and
    /// returns whether this call won. Only the winner may push, so two
    /// concurrent approvals open the change once, not once each.
    pub fn begin_publish(&self, id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE review SET state='publishing', updated_ms=?2
             WHERE id=?1 AND state IN ('new','claimed','revising')",
            rusqlite::params![id, now_ms()],
        )?;
        Ok(n == 1)
    }

    /// Complete a publish: record the approved bytes and where they landed. Only
    /// a row this call moved into `publishing` is finished.
    pub fn finish_publish(
        &self,
        id: &str,
        title: &str,
        body: &str,
        publish_result: &str,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE review SET state='approved', edited_title=?2, edited_body=?3,
                publish_result=?4, resolved_mono_ms=0, updated_ms=?5
             WHERE id=?1 AND state='publishing'",
            rusqlite::params![id, title, body, publish_result, now_ms()],
        )?;
        Ok(n == 1)
    }

    /// Undo a publish claim when the forge refused, so the review returns to the
    /// queue rather than being stuck mid-publish.
    pub fn abort_publish(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE review SET state='claimed', updated_ms=?2
             WHERE id=?1 AND state='publishing'",
            rusqlite::params![id, now_ms()],
        )?;
        Ok(())
    }

    /// Release a claim: the operator navigated away or their client went quiet.
    /// A claim measures attention, so one that never releases would report every
    /// review as attended forever.
    pub fn release_review(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE review SET state='new', claimed_ms=NULL, updated_ms=?2
             WHERE id=?1 AND state='claimed'",
            rusqlite::params![id, now_ms()],
        )?;
        Ok(())
    }

    /// Claims older than the grace period, for the sweeper. A dropped socket
    /// should not zero the attention count; a closed laptop should.
    pub fn stale_claims(&self, older_than_ms: i64) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let cutoff = now_ms() - older_than_ms;
        let mut stmt = conn.prepare(
            "SELECT id FROM review WHERE state='claimed' AND COALESCE(updated_ms, 0) < ?1",
        )?;
        let rows = stmt
            .query_map([cutoff], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// Changes requested: the review stays in the queue, marked so the operator
    /// can see it is waiting on the agent rather than on them. Returns false if
    /// the review was no longer awaiting a verdict.
    pub fn request_changes(&self, id: &str, notes: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE review SET state='revising', verdict_reason=?2, updated_ms=?3
             WHERE id=?1 AND state IN ('new','claimed')",
            rusqlite::params![id, notes, now_ms()],
        )?;
        Ok(n == 1)
    }

    /// A resubmission keeps the same review, so the operator sees one evolving
    /// thread rather than a new card each time.
    pub fn revise_review(
        &self,
        id: &str,
        diff: &str,
        files: &str,
        head_sha: &str,
        added: i64,
        removed: i64,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE review SET diff=?2, files=?3, head_sha=?4, added=?5, removed=?6, state='new',
                verdict_reason=NULL, claimed_ms=NULL, resolved_mono_ms=NULL, updated_ms=?7
             WHERE id=?1",
            rusqlite::params![id, diff, files, head_sha, added, removed, now_ms()],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(store: &Store) -> String {
        let n = NodeRow {
            id: "n1".into(),
            name: "test".into(),
            state: "ready".into(),
            failed_check: None,
            failed_detail: None,
            harness_id: "omp".into(),
            harness_pinned: "18.0.4".into(),
            harness_found: Some("18.0.4".into()),
            models_json: None,
            checked_at_ms: Some(now_ms()),
            is_self: 1,
            x25519_pub: None,
            last_seen_ms: None,
            reachable: 1,
        };
        store.put_node(&n).unwrap();
        n.id
    }

    fn session(store: &Store, id: &str, node_id: &str) {
        store
            .insert_session(&SessionRow {
                id: id.into(),
                node_id: node_id.into(),
                channel: "personal".into(),
                work_item_id: None,
                repo_path: "/repo".into(),
                worktree_path: None,
                branch: "feat/x".into(),
                harness_id: "omp".into(),
                harness_version: "18.0.4".into(),
                harness_session_id: None,
                container_name: None,
                model: "m".into(),
                budget_tokens: 1000,
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
                started_mono_ms: Some(0),
                ended_mono_ms: None,
                updated_ms: now_ms(),
            })
            .unwrap();
    }

    #[test]
    fn migration_is_idempotent() {
        // Re-running migrate on the same connection must not fail.
        let store = Store::open_in_memory().unwrap();
        let conn = store.conn.lock().unwrap();
        schema::migrate(&conn).unwrap();
        schema::migrate(&conn).unwrap();
    }

    #[test]
    fn event_seq_is_monotonic() {
        let store = Store::open_in_memory().unwrap();
        let nid = node(&store);
        session(&store, "s1", &nid);
        let mut last = 0;
        for i in 0..5 {
            let seq = store
                .append_event(&NewEvent {
                    session_id: "s1".into(),
                    work_item_id: None,
                    kind: "message".into(),
                    ref_id: None,
                    payload: serde_json::json!({ "i": i }),
                    at_ms: now_ms(),
                    mono_ms: i,
                })
                .unwrap();
            assert!(seq > last);
            last = seq;
        }
        assert_eq!(store.events_after("s1", 0, 100).unwrap().len(), 5);
        assert_eq!(store.events_after("s1", last - 1, 100).unwrap().len(), 1);
    }

    #[test]
    fn permission_resolves_once() {
        let store = Store::open_in_memory().unwrap();
        let nid = node(&store);
        session(&store, "s1", &nid);
        let p = PermissionRow {
            id: "p1".into(),
            session_id: "s1".into(),
            node_id: nid,
            rpc_id: 0,
            tool_call_id: Some("call|fc".into()),
            title: "run just test".into(),
            kind: Some("execute".into()),
            raw_input: None,
            options: "[]".into(),
            state: "new".into(),
            answer_option_id: None,
            created_ms: now_ms(),
            created_mono_ms: 10,
            resolved_mono_ms: None,
            expires_ms: now_ms() + 60_000,
        };
        store.insert_permission(&p).unwrap();
        assert_eq!(store.open_permissions().unwrap().len(), 1);
        assert!(store
            .resolve_permission("p1", "answered", Some("reject_once"), 50)
            .unwrap());
        // A second resolve is refused: the request is no longer `new`.
        assert!(!store
            .resolve_permission("p1", "answered", Some("allow_once"), 60)
            .unwrap());
        assert_eq!(store.open_permissions().unwrap().len(), 0);
    }

    #[test]
    fn session_patch_writes_only_set_fields() {
        let store = Store::open_in_memory().unwrap();
        let nid = node(&store);
        session(&store, "s1", &nid);
        store
            .update_session(
                "s1",
                SessionPatch {
                    tokens_used: Some(1500),
                    ..SessionPatch::state("killed_budget")
                },
            )
            .unwrap();
        let s = store.get_session("s1").unwrap().unwrap();
        assert_eq!(s.state, "killed_budget");
        assert_eq!(s.tokens_used, 1500);
        assert_eq!(s.branch, "feat/x"); // untouched
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    /// A database as the Phase 1 build left it, migrated forward.
    #[test]
    fn v2_database_migrates_and_rekeys() {
        let conn = Connection::open_in_memory().unwrap();
        schema::migrate_to(&conn, 2).unwrap();
        conn.execute_batch(
            "INSERT INTO node (id, name, state, harness_id, harness_pinned) VALUES ('u1','n','ready','omp','1');
             INSERT INTO session (id, node_id, channel, repo_path, branch, harness_id, harness_version, model,
                budget_tokens, state, created_ms, updated_ms)
                VALUES ('s1','u1','personal','/r','b','omp','1','m',10,'closed',0,0);
             INSERT INTO event (session_id, kind, payload, at_ms, mono_ms) VALUES ('s1','state','{}',0,0);
             INSERT INTO permission_request (id, session_id, node_id, rpc_id, title, options, state, created_ms,
                created_mono_ms, expires_ms) VALUES ('p1','s1','',0,'t','[]','expired',0,0,0);",
        )
        .unwrap();
        schema::migrate(&conn).unwrap();
        let store = Store {
            conn: Mutex::new(conn),
        };
        assert_eq!(store.self_node_id().unwrap().as_deref(), Some("u1"));
        let ev = store.events_after("s1", -1, 10).unwrap();
        assert_eq!(ev[0].node_id, "u1");

        store.rekey_self_node("u1", "abcd").unwrap();
        assert_eq!(store.self_node_id().unwrap().as_deref(), Some("abcd"));
        assert_eq!(store.get_session("s1").unwrap().unwrap().node_id, "abcd");
        assert_eq!(store.events_after("s1", -1, 10).unwrap()[0].node_id, "abcd");
        assert_eq!(store.get_permission("p1").unwrap().unwrap().node_id, "abcd");
        // Idempotent.
        store.rekey_self_node("abcd", "abcd").unwrap();
        assert_eq!(store.list_nodes().unwrap().len(), 1);
    }

    #[test]
    fn append_event_derives_node_id_from_session() {
        let store = Store::open_in_memory().unwrap();
        store
            .put_node(&NodeRow {
                id: "n1".into(),
                name: "n".into(),
                state: "ready".into(),
                failed_check: None,
                failed_detail: None,
                harness_id: "omp".into(),
                harness_pinned: "1".into(),
                harness_found: None,
                models_json: None,
                checked_at_ms: None,
                is_self: 1,
                x25519_pub: None,
                last_seen_ms: None,
                reachable: 1,
            })
            .unwrap();
        let conn = store.conn.lock().unwrap();
        conn.execute_batch(
            "INSERT INTO session (id, node_id, channel, repo_path, branch, harness_id, harness_version, model,
                budget_tokens, state, created_ms, updated_ms)
                VALUES ('s1','n1','personal','/r','b','omp','1','m',10,'running',0,0);",
        )
        .unwrap();
        drop(conn);
        store
            .append_event(&NewEvent {
                session_id: "s1".into(),
                work_item_id: None,
                kind: "state".into(),
                ref_id: None,
                payload: serde_json::json!({}),
                at_ms: 0,
                mono_ms: 0,
            })
            .unwrap();
        assert_eq!(store.all_events_after(-1, 10).unwrap()[0].node_id, "n1");
    }
}
