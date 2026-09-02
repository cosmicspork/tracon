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

pub mod corpus;
pub mod vectors;
pub use corpus::*;
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
    #[error("{0}")]
    Invalid(String),
}

type Result<T> = std::result::Result<T, StoreError>;

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Register `sqlite-vec` with SQLite itself, once per process, before any
/// connection is opened. It is compiled in rather than loaded at run time, so
/// there is no `.so` to ship beside the binary and nothing to go missing on a
/// host — which is what keeps the single static binary a single static binary.
///
/// The signature is spelled with `c_char` rather than `i8` deliberately: it is
/// signed on x86_64 and unsigned on aarch64, and both are release targets.
fn register_vec() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
            *const (),
            unsafe extern "C" fn(
                *mut rusqlite::ffi::sqlite3,
                *mut *mut std::os::raw::c_char,
                *const rusqlite::ffi::sqlite3_api_routines,
            ) -> i32,
        >(
            sqlite_vec::sqlite3_vec_init as *const ()
        )));
    });
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        register_vec();
        let conn = Connection::open(path)?;
        schema::migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_in_memory() -> Result<Self> {
        register_vec();
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
                last_seen_ms, reachable, providers_json)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)
             ON CONFLICT(id) DO UPDATE SET name=?2, state=?3, failed_check=?4, failed_detail=?5,
                harness_id=?6, harness_pinned=?7, harness_found=?8, models_json=?9, checked_at_ms=?10,
                is_self=?11, x25519_pub=?12, last_seen_ms=?13, reachable=?14, providers_json=?15",
            rusqlite::params![
                n.id, n.name, n.state, n.failed_check, n.failed_detail, n.harness_id,
                n.harness_pinned, n.harness_found, n.models_json, n.checked_at_ms, n.is_self,
                n.x25519_pub, n.last_seen_ms, n.reachable, n.providers_json
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

    /// Update only this node's provider summary, for the hello and the mirror.
    pub fn set_node_providers(&self, id: &str, providers_json: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE node SET providers_json=?2 WHERE id=?1",
            [id, providers_json],
        )?;
        Ok(())
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
                ended_mono_ms, updated_ms, project_id, phase, policy_version, review_id)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,
                ?23,?24,?25,?26,?27,?28,?29,?30,?31)",
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
                s.updated_ms,
                s.project_id,
                s.phase,
                s.policy_version,
                s.review_id
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

    /// Every session that ever held an item, oldest first.
    pub fn sessions_of_work_item(&self, work_item_id: &str) -> Result<Vec<SessionRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT * FROM session WHERE work_item_id=?1 ORDER BY created_ms")?;
        let rows = stmt
            .query_map([work_item_id], SessionRow::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Every session, or every session in one state. Archived rows are
    /// included: this is the whole history, and the screens that want the
    /// short list ask [`Store::queue_sessions`] instead.
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

    /// What the home shows: sessions still going, and the last few that ended
    /// and have not been put away. Both bounds are the database's, so a node
    /// with ten thousand ended sessions reads the same number of rows as a new
    /// one. The terminal states are `SessionState::is_terminal`, spelled out
    /// here because SQL cannot ask it.
    pub fn queue_sessions(&self, ended_limit: usize) -> Result<(Vec<SessionRow>, Vec<SessionRow>)> {
        const TERMINAL: &str = "('closed','killed_budget','failed')";
        let conn = self.conn.lock().unwrap();
        let mut running = conn.prepare(&format!(
            "SELECT * FROM session WHERE state NOT IN {TERMINAL} ORDER BY created_ms DESC"
        ))?;
        let running: Vec<SessionRow> = running
            .query_map([], SessionRow::from_row)?
            .collect::<std::result::Result<_, _>>()?;
        let mut ended = conn.prepare(&format!(
            "SELECT * FROM session WHERE state IN {TERMINAL} AND archived_ms IS NULL \
             ORDER BY created_ms DESC LIMIT ?1"
        ))?;
        let ended: Vec<SessionRow> = ended
            .query_map([ended_limit as i64], SessionRow::from_row)?
            .collect::<std::result::Result<_, _>>()?;
        Ok((running, ended))
    }

    /// Put one session away, or bring it back. Returns the row as it now is,
    /// so the caller can publish it.
    pub fn set_session_archived(&self, id: &str, ms: Option<i64>) -> Result<Option<SessionRow>> {
        {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "UPDATE session SET archived_ms=?2 WHERE id=?1",
                rusqlite::params![id, ms],
            )?;
        }
        self.get_session(id)
    }

    /// Put away everything that has ended. A running session is left alone:
    /// archiving is for what is over.
    pub fn archive_ended_sessions(&self, ms: i64) -> Result<Vec<SessionRow>> {
        let ids: Vec<String> = {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT id FROM session WHERE state IN ('closed','killed_budget','failed') \
                 AND archived_ms IS NULL",
            )?;
            let ids = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            conn.execute(
                "UPDATE session SET archived_ms=?1 WHERE state IN ('closed','killed_budget','failed') \
                 AND archived_ms IS NULL",
                [ms],
            )?;
            ids
        };
        let mut rows = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(r) = self.get_session(&id)? {
                rows.push(r);
            }
        }
        Ok(rows)
    }

    /// Every repository sessions have run against, most recently used first.
    /// The session table is never pruned, so this is the node's whole memory
    /// of where work happens.
    pub fn recent_repos(&self, limit: usize) -> Result<Vec<RecentRepo>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT repo_path, MAX(created_ms), COUNT(*) FROM session \
             GROUP BY repo_path ORDER BY 2 DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([limit as i64], |row| {
                Ok(RecentRepo {
                    repo_path: row.get(0)?,
                    last_used_ms: row.get(1)?,
                    sessions: row.get(2)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
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
        // The owning node and work item are the session's; derived here so
        // no call site has to carry them, and per-item metrics are a group-by.
        conn.execute(
            "INSERT INTO event (session_id, work_item_id, kind, ref_id, payload, at_ms, mono_ms, node_id)
             VALUES (?1, COALESCE(?2, (SELECT work_item_id FROM session WHERE id=?1)),
                     ?3,?4,?5,?6,?7, (SELECT node_id FROM session WHERE id=?1))",
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

    // ---- model usage ----

    pub fn record_usage(&self, u: &UsageRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO model_usage (channel, node_id, session_id, provider, model, at_ms, input_tokens, output_tokens, requests)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                u.channel,
                u.node_id,
                u.session_id,
                u.provider,
                u.model,
                u.at_ms,
                u.input_tokens,
                u.output_tokens,
                u.requests
            ],
        )?;
        Ok(())
    }

    /// Tokens (input + output) a channel spent since `since_ms`, as the
    /// gateway counted them on this node.
    pub fn usage_tokens_since(&self, channel: &str, since_ms: i64) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COALESCE(SUM(input_tokens + output_tokens), 0) FROM model_usage
             WHERE channel = ?1 AND at_ms >= ?2",
            rusqlite::params![channel, since_ms],
            |r| r.get(0),
        )
        .map_err(Into::into)
    }

    /// Per-session usage since `since_ms` on a channel: (session_id, provider,
    /// input, output, requests).
    pub fn usage_by_session(&self, channel: &str, since_ms: i64) -> Result<Vec<SessionUsage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT COALESCE(session_id, ''), provider, SUM(input_tokens), SUM(output_tokens), SUM(requests)
             FROM model_usage WHERE channel = ?1 AND at_ms >= ?2
             GROUP BY session_id, provider",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![channel, since_ms], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// Reviews on a channel decided since `since_ms`, any final state.
    pub fn reviews_decided_since(&self, channel: &str, since_ms: i64) -> Result<Vec<ReviewRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM review WHERE channel = ?1 AND state IN ('approved', 'rejected')
               AND updated_ms >= ?2 ORDER BY updated_ms",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![channel, since_ms], ReviewRow::from_row)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// Permission requests on a channel's sessions answered since `since_ms`:
    /// (count, seconds of human latency, request→answer, monotonic on the node
    /// that observed both).
    pub fn permissions_answered_since(&self, channel: &str, since_ms: i64) -> Result<(i64, f64)> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(MAX(p.resolved_mono_ms - p.created_mono_ms, 0)), 0) / 1000.0
             FROM permission_request p JOIN session s ON s.id = p.session_id
             WHERE s.channel = ?1 AND p.answer_option_id IS NOT NULL AND p.created_ms >= ?2",
            rusqlite::params![channel, since_ms],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(Into::into)
    }

    /// Sessions on a channel created since `since_ms`.
    pub fn sessions_on_channel_since(
        &self,
        channel: &str,
        since_ms: i64,
    ) -> Result<Vec<SessionRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM session WHERE channel = ?1 AND created_ms >= ?2 ORDER BY created_ms",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![channel, since_ms], SessionRow::from_row)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// The review a commit came from: by the reviewed sha (a prefix will do)
    /// or by where it was published.
    pub fn review_by_sha(&self, sha: &str) -> Result<Option<ReviewRow>> {
        let conn = self.conn.lock().unwrap();
        let like = format!("{sha}%");
        let anywhere = format!("%{sha}%");
        conn.query_row(
            "SELECT * FROM review WHERE head_sha LIKE ?1 OR publish_result LIKE ?2
             ORDER BY updated_ms DESC LIMIT 1",
            rusqlite::params![like, anywhere],
            ReviewRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn has_event(&self, session_id: &str, kind: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM event WHERE session_id = ?1 AND kind = ?2",
            rusqlite::params![session_id, kind],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Totals per provider and model since `since_ms`, for one channel or all.
    pub fn usage_since(&self, channel: Option<&str>, since_ms: i64) -> Result<Vec<UsageTotal>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT channel, provider, model, SUM(requests), SUM(input_tokens), SUM(output_tokens)
             FROM model_usage WHERE at_ms >= ?1 AND (?2 IS NULL OR channel = ?2)
             GROUP BY channel, provider, model ORDER BY channel, provider, model",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![since_ms, channel], |r| {
                Ok(UsageTotal {
                    channel: r.get(0)?,
                    provider: r.get(1)?,
                    model: r.get(2)?,
                    requests: r.get(3)?,
                    input_tokens: r.get(4)?,
                    output_tokens: r.get(5)?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    // ---- mesh: channels, cursors, outbox, seen ----

    pub fn channel_put(&self, name: &str, keyring: &[u8], bindings_json: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = now_ms();
        conn.execute(
            "INSERT INTO channel (name, keyring, bindings_json, created_ms, updated_ms)
             VALUES (?1,?2,?3,?4,?4)
             ON CONFLICT(name) DO UPDATE SET keyring=?2, bindings_json=?3, updated_ms=?4",
            rusqlite::params![name, keyring, bindings_json, now],
        )?;
        Ok(())
    }

    pub fn channel_get(&self, name: &str) -> Result<Option<ChannelRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT * FROM channel WHERE name=?1",
            [name],
            ChannelRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn channel_list(&self) -> Result<Vec<ChannelRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM channel ORDER BY name")?;
        let rows = stmt
            .query_map([], ChannelRow::from_row)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// Replace the set of channels a node is bound to.
    pub fn node_channels_set(&self, node_id: &str, channels: &[String]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM node_channel WHERE node_id=?1", [node_id])?;
        for c in channels {
            tx.execute(
                "INSERT OR IGNORE INTO node_channel (node_id, channel) VALUES (?1,?2)",
                [node_id, c],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn node_channel_add(&self, node_id: &str, channel: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO node_channel (node_id, channel) VALUES (?1,?2)",
            [node_id, channel],
        )?;
        Ok(())
    }

    pub fn node_channels(&self, node_id: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT channel FROM node_channel WHERE node_id=?1 ORDER BY channel")?;
        let rows = stmt
            .query_map([node_id], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    pub fn nodes_in_channel(&self, channel: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT node_id FROM node_channel WHERE channel=?1 ORDER BY node_id")?;
        let rows = stmt
            .query_map([channel], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    pub fn cursor_get(&self, channel: &str) -> Result<u64> {
        let conn = self.conn.lock().unwrap();
        let v: Option<i64> = conn
            .query_row(
                "SELECT seq FROM mesh_cursor WHERE channel=?1",
                [channel],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v.unwrap_or(0).max(0) as u64)
    }

    pub fn cursor_set(&self, channel: &str, seq: u64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO mesh_cursor (channel, seq) VALUES (?1,?2)
             ON CONFLICT(channel) DO UPDATE SET seq=?2",
            rusqlite::params![channel, seq as i64],
        )?;
        Ok(())
    }

    pub fn outbox_push(&self, channel: &str, envelope: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO mesh_outbox (channel, envelope, created_ms) VALUES (?1,?2,?3)",
            rusqlite::params![channel, envelope, now_ms()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Oldest first: `(id, channel, envelope)`.
    pub fn outbox_peek(&self, limit: i64) -> Result<Vec<(i64, String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT id, channel, envelope FROM mesh_outbox ORDER BY id LIMIT ?1")?;
        let rows = stmt
            .query_map([limit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    pub fn outbox_delete(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM mesh_outbox WHERE id=?1", [id])?;
        Ok(())
    }

    pub fn outbox_len(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM mesh_outbox", [], |r| r.get(0))?;
        Ok(n as usize)
    }

    /// `true` if the frame id was not seen before.
    pub fn seen_insert(&self, frame_id: &str, at_ms: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "INSERT OR IGNORE INTO mesh_seen (frame_id, at_ms) VALUES (?1,?2)",
            rusqlite::params![frame_id, at_ms],
        )?;
        Ok(n == 1)
    }

    pub fn seen_prune(&self, older_than_ms: i64) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute("DELETE FROM mesh_seen WHERE at_ms < ?1", [older_than_ms])?;
        Ok(n)
    }

    // ---- mesh: mirrored rows ----

    /// A minimal row for a peer we have rows about but no hello from yet, so
    /// foreign keys hold. A real hello replaces it.
    pub fn ensure_peer_node(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO node (id, name, state, harness_id, harness_pinned, is_self, reachable)
             VALUES (?1, '', 'unknown', '', '', 0, 0)",
            [id],
        )?;
        Ok(())
    }

    /// Mark a peer reachable or not; returns whether the flag changed.
    pub fn set_reachable(&self, id: &str, reachable: bool) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE node SET reachable=?2 WHERE id=?1 AND reachable<>?2",
            rusqlite::params![id, reachable as i64],
        )?;
        Ok(n == 1)
    }

    /// Insert or fully replace a peer's session, except the local draft.
    pub fn upsert_session_mirror(&self, s: &SessionRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO session (id, node_id, channel, work_item_id, repo_path, worktree_path,
                branch, harness_id, harness_version, harness_session_id, container_name, model,
                budget_tokens, tokens_used, cost_usd, context_used, context_size, state, end_reason,
                last_error, turn_active, draft, draft_updated_ms, created_ms, started_mono_ms,
                ended_mono_ms, updated_ms, project_id, phase, policy_version, review_id)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,NULL,
                NULL,?22,?23,?24,?25,?26,?27,?28,?29)
             ON CONFLICT(id) DO UPDATE SET node_id=?2, channel=?3, work_item_id=?4, repo_path=?5,
                worktree_path=?6, branch=?7, harness_id=?8, harness_version=?9,
                harness_session_id=?10, container_name=?11, model=?12, budget_tokens=?13,
                tokens_used=?14, cost_usd=?15, context_used=?16, context_size=?17, state=?18,
                end_reason=?19, last_error=?20, turn_active=?21, created_ms=?22,
                started_mono_ms=?23, ended_mono_ms=?24, updated_ms=?25, project_id=?26,
                phase=?27, policy_version=?28, review_id=?29",
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
                s.created_ms,
                s.started_mono_ms,
                s.ended_mono_ms,
                s.updated_ms,
                s.project_id,
                s.phase,
                s.policy_version,
                s.review_id
            ],
        )?;
        Ok(())
    }

    /// Append a peer's event under its origin seq. `None` if already present.
    pub fn append_mirrored_event(
        &self,
        node_id: &str,
        origin_seq: i64,
        e: &NewEvent,
    ) -> Result<Option<i64>> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "INSERT OR IGNORE INTO event (session_id, work_item_id, kind, ref_id, payload, at_ms,
                mono_ms, node_id, origin_seq)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            rusqlite::params![
                e.session_id,
                e.work_item_id,
                e.kind,
                e.ref_id,
                serde_json::to_string(&e.payload)?,
                e.at_ms,
                e.mono_ms,
                node_id,
                origin_seq
            ],
        )?;
        Ok((n == 1).then(|| conn.last_insert_rowid()))
    }

    /// The highest origin seq mirrored for a peer's session, if any.
    pub fn mirrored_origin_max(&self, session_id: &str) -> Result<Option<i64>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT MAX(origin_seq) FROM event WHERE session_id=?1",
            [session_id],
            |r| r.get(0),
        )
        .map_err(Into::into)
    }

    pub fn upsert_permission_mirror(&self, p: &PermissionRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO permission_request (id, session_id, node_id, rpc_id, tool_call_id, title,
                kind, raw_input, options, state, answer_option_id, created_ms, created_mono_ms,
                resolved_mono_ms, expires_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)
             ON CONFLICT(id) DO UPDATE SET state=?10, answer_option_id=?11, resolved_mono_ms=?14,
                expires_ms=?15, title=?6, options=?9",
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

    /// Open permission rows of `node_id` in `channel` not in `keep` are
    /// expired: the owner no longer lists them as waiting.
    pub fn expire_absent_permissions(
        &self,
        node_id: &str,
        channel: &str,
        keep: &[String],
    ) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT p.id FROM permission_request p JOIN session s ON s.id = p.session_id
             WHERE p.node_id=?1 AND s.channel=?2 AND p.state='new'",
        )?;
        let open: Vec<String> = stmt
            .query_map([node_id, channel], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        let mut n = 0;
        for id in open.into_iter().filter(|id| !keep.contains(id)) {
            n += conn.execute(
                "UPDATE permission_request SET state='expired' WHERE id=?1 AND state='new'",
                [&id],
            )?;
        }
        Ok(n)
    }

    pub fn upsert_review_mirror(&self, r: &ReviewRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO review (id, session_id, node_id, channel, kind, title, body, edited_title,
                edited_body, provider, target, diff, files, head_sha, base_ref, added, removed, state,
                verdict_reason, publish_result, claimed_ms, created_ms, created_mono_ms,
                resolved_mono_ms, updated_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,
                ?23,?24,?25)
             ON CONFLICT(id) DO UPDATE SET title=?6, body=?7, edited_title=?8, edited_body=?9,
                diff=?12, files=?13, head_sha=?14, added=?16, removed=?17, state=?18,
                verdict_reason=?19, publish_result=?20, claimed_ms=?21, resolved_mono_ms=?24,
                updated_ms=?25",
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

    /// Open reviews of `node_id` in `channel` not in `keep` are marked gone:
    /// the owner no longer lists them.
    pub fn gone_absent_reviews(
        &self,
        node_id: &str,
        channel: &str,
        keep: &[String],
    ) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id FROM review WHERE node_id=?1 AND channel=?2
             AND state IN ('new','claimed','revising')",
        )?;
        let open: Vec<String> = stmt
            .query_map([node_id, channel], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        let mut n = 0;
        for id in open.into_iter().filter(|id| !keep.contains(id)) {
            n += conn.execute(
                "UPDATE review SET state='gone', updated_ms=?2 WHERE id=?1",
                rusqlite::params![id, now_ms()],
            )?;
        }
        Ok(n)
    }

    pub fn sessions_of_node(&self, node_id: &str) -> Result<Vec<SessionRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT * FROM session WHERE node_id=?1 ORDER BY created_ms DESC")?;
        let rows = stmt
            .query_map([node_id], SessionRow::from_row)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// Non-terminal sessions of `node_id` in `channel` absent from its snapshot
    /// are closed: the owner lost them. Returns the ids closed.
    pub fn close_absent_sessions(
        &self,
        node_id: &str,
        channel: &str,
        keep: &[String],
    ) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id FROM session WHERE node_id=?1 AND channel=?2
             AND state NOT IN ('closed','killed_budget','failed')",
        )?;
        let open: Vec<String> = stmt
            .query_map([node_id, channel], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        let mut closed = Vec::new();
        for id in open.into_iter().filter(|id| !keep.contains(id)) {
            conn.execute(
                "UPDATE session SET state='closed', end_reason='harness_exit',
                    last_error='lost on owner', turn_active=0, updated_ms=?2 WHERE id=?1",
                rusqlite::params![id, now_ms()],
            )?;
            closed.push(id);
        }
        Ok(closed)
    }
}

impl Store {}

/// One model request as the gateway saw it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRow {
    pub channel: String,
    pub node_id: String,
    pub session_id: Option<String>,
    pub provider: String,
    pub model: Option<String>,
    pub at_ms: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub requests: i64,
}

/// `(session_id, provider, input_tokens, output_tokens, requests)`.
pub type SessionUsage = (String, String, i64, i64, i64);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageTotal {
    pub channel: String,
    pub provider: String,
    pub model: Option<String>,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
}

/// The kv key holding the SHA-256 of the operator token. The token itself is
/// shown once by `tracon auth issue` and never stored anywhere.
pub const OPERATOR_TOKEN_KEY: &str = "operator_token_hash";

mod records {
    use super::*;

    /// One logged-in client. Keyed by the hash of its cookie, so a database
    /// read cannot mint a session.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct AuthSessionRow {
        pub token_hash: String,
        pub created_ms: i64,
        pub last_seen_ms: i64,
        pub expires_ms: i64,
        pub user_agent: Option<String>,
    }

    /// A phone (or any browser) this node pushes to. Keys are kept as the
    /// browser gave them, base64url.
    #[derive(Debug, Clone)]
    pub struct PushSubscriptionRow {
        pub id: String,
        /// The `auth_session` that registered it; `None` for a browser on
        /// this machine, which never logs in.
        pub session_hash: Option<String>,
        pub endpoint: String,
        pub p256dh: String,
        pub auth: String,
        pub user_agent: Option<String>,
        pub created_ms: i64,
        pub last_ok_ms: Option<i64>,
        pub fail_count: i64,
    }

    impl PushSubscriptionRow {
        pub fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
            Ok(Self {
                id: r.get("id")?,
                session_hash: r.get("session_hash")?,
                endpoint: r.get("endpoint")?,
                p256dh: r.get("p256dh")?,
                auth: r.get("auth")?,
                user_agent: r.get("user_agent")?,
                created_ms: r.get("created_ms")?,
                last_ok_ms: r.get("last_ok_ms")?,
                fail_count: r.get("fail_count")?,
            })
        }
    }

    impl AuthSessionRow {
        pub(super) fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Self> {
            Ok(Self {
                token_hash: r.get("token_hash")?,
                created_ms: r.get("created_ms")?,
                last_seen_ms: r.get("last_seen_ms")?,
                expires_ms: r.get("expires_ms")?,
                user_agent: r.get("user_agent")?,
            })
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ChannelRow {
        pub name: String,
        pub keyring: Vec<u8>,
        pub bindings_json: String,
        pub created_ms: i64,
        pub updated_ms: i64,
    }

    impl ChannelRow {
        pub(super) fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Self> {
            Ok(Self {
                name: r.get("name")?,
                keyring: r.get("keyring")?,
                bindings_json: r.get("bindings_json")?,
                created_ms: r.get("created_ms")?,
                updated_ms: r.get("updated_ms")?,
            })
        }
    }

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
        /// The node's provider summary as its `Providers::list()` reports it,
        /// carried in the hello like `models_json`. NULL from older builds.
        #[serde(default)]
        pub providers_json: Option<String>,
    }

    fn one() -> i64 {
        1
    }

    impl NodeRow {
        /// The wire shape the interface and the mesh share.
        pub fn to_json(&self) -> Value {
            let models: Value = self
                .models_json
                .as_deref()
                .and_then(|m| serde_json::from_str(m).ok())
                .unwrap_or_else(|| serde_json::json!([]));
            serde_json::json!({
                "id": self.id,
                "name": self.name,
                "state": self.state,
                "failed_check": self.failed_check,
                "failed_detail": self.failed_detail,
                "harness": {
                    "id": self.harness_id,
                    "pinned": self.harness_pinned,
                    "found": self.harness_found,
                    "mismatch": self.harness_found.as_ref().map(|f| f != &self.harness_pinned).unwrap_or(false),
                },
                "models": models,
                "checked_at_ms": self.checked_at_ms,
                "is_self": self.is_self != 0,
                "reachable": self.reachable != 0,
                "last_seen_ms": self.last_seen_ms,
                "x25519_pub": self.x25519_pub,
                "providers": self
                    .providers_json
                    .as_deref()
                    .and_then(|p| serde_json::from_str::<Value>(p).ok()),
            })
        }

        /// A peer's row from its own wire shape. `is_self`, `reachable`, and
        /// `last_seen_ms` are the receiver's to set, never the sender's.
        pub fn from_json(v: &Value) -> Option<Self> {
            let s = |k: &str| v.get(k).and_then(Value::as_str).map(String::from);
            Some(Self {
                id: s("id")?,
                name: s("name").unwrap_or_default(),
                state: s("state").unwrap_or_else(|| "unknown".into()),
                failed_check: s("failed_check"),
                failed_detail: s("failed_detail"),
                harness_id: v["harness"]["id"].as_str().unwrap_or("").to_string(),
                harness_pinned: v["harness"]["pinned"].as_str().unwrap_or("").to_string(),
                harness_found: v["harness"]["found"].as_str().map(String::from),
                models_json: v.get("models").map(|m| m.to_string()),
                checked_at_ms: v.get("checked_at_ms").and_then(Value::as_i64),
                is_self: 0,
                x25519_pub: s("x25519_pub"),
                last_seen_ms: None,
                reachable: 1,
                providers_json: v
                    .get("providers")
                    .filter(|p| !p.is_null())
                    .map(|p| p.to_string()),
            })
        }
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
                providers_json: r.get("providers_json")?,
            })
        }
    }

    /// One repository the node has run sessions against.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct RecentRepo {
        pub repo_path: String,
        pub last_used_ms: i64,
        pub sessions: i64,
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
        #[serde(default)]
        pub project_id: Option<String>,
        /// `plan`, `execute`, or `review`.
        #[serde(default = "default_phase")]
        pub phase: String,
        /// The policy bundle version the session started under.
        #[serde(default)]
        pub policy_version: Option<i64>,
        /// For a review session: the review it was spawned to read.
        #[serde(default)]
        pub review_id: Option<String>,
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
        /// When the operator put this session away. Presentation only: the
        /// state and the phase are untouched, and nothing about the session's
        /// history changes. NULL means it is still on the home.
        #[serde(default)]
        pub archived_ms: Option<i64>,
    }

    fn default_phase() -> String {
        "execute".into()
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
                project_id: r.get("project_id")?,
                phase: r.get("phase")?,
                policy_version: r.get("policy_version")?,
                review_id: r.get("review_id")?,
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
                archived_ms: r.get("archived_ms")?,
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
        /// The deterministic checks that passed at submit.
        #[serde(default)]
        pub checks_json: Option<String>,
        /// The fresh session that read this review, when one was spawned.
        #[serde(default)]
        pub review_session_id: Option<String>,
        /// That session's verdict: `{verdict, summary, findings}`.
        #[serde(default)]
        pub ai_verdict_json: Option<String>,
        /// A unified diff the operator edited by hand, carried back to the
        /// agent with the notes. The agent applies it and resubmits; nothing
        /// but the agent writes to the worktree.
        #[serde(default)]
        pub revision_patch: Option<String>,
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
                checks_json: r.get("checks_json")?,
                review_session_id: r.get("review_session_id")?,
                ai_verdict_json: r.get("ai_verdict_json")?,
                revision_patch: r.get("revision_patch")?,
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
    // ---- operator auth ----

    pub fn kv_get(&self, k: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT v FROM kv WHERE k=?1", [k], |r| r.get(0))
            .optional()
            .map_err(Into::into)
    }

    pub fn kv_put(&self, k: &str, v: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO kv (k, v, updated_ms) VALUES (?1,?2,?3)
             ON CONFLICT(k) DO UPDATE SET v=excluded.v, updated_ms=excluded.updated_ms",
            rusqlite::params![k, v, now_ms()],
        )?;
        Ok(())
    }

    pub fn kv_delete(&self, k: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM kv WHERE k=?1", [k])?;
        Ok(())
    }

    /// Set the operator token hash and drop every logged-in client in one
    /// transaction: rotating the token must not leave an old cookie working.
    pub fn set_operator_token(&self, hash: Option<&str>) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        match hash {
            Some(h) => {
                tx.execute(
                    "INSERT INTO kv (k, v, updated_ms) VALUES (?1,?2,?3)
                     ON CONFLICT(k) DO UPDATE SET v=excluded.v, updated_ms=excluded.updated_ms",
                    rusqlite::params![OPERATOR_TOKEN_KEY, h, now_ms()],
                )?;
            }
            None => {
                tx.execute("DELETE FROM kv WHERE k=?1", [OPERATOR_TOKEN_KEY])?;
            }
        }
        tx.execute("DELETE FROM auth_session", [])?;
        // The devices those clients registered go with them; a browser on
        // this machine has no session and stays.
        tx.execute(
            "DELETE FROM push_subscription WHERE session_hash IS NOT NULL",
            [],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn auth_session_insert(&self, r: &AuthSessionRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO auth_session (token_hash, created_ms, last_seen_ms, expires_ms, user_agent)
             VALUES (?1,?2,?3,?4,?5)",
            rusqlite::params![
                r.token_hash,
                r.created_ms,
                r.last_seen_ms,
                r.expires_ms,
                r.user_agent
            ],
        )?;
        Ok(())
    }

    /// The row for a presented cookie, if it exists and has not expired.
    pub fn auth_session_live(&self, token_hash: &str, now: i64) -> Result<Option<AuthSessionRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT * FROM auth_session WHERE token_hash=?1 AND expires_ms > ?2",
            rusqlite::params![token_hash, now],
            AuthSessionRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn auth_session_touch(&self, token_hash: &str, now: i64, expires_ms: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE auth_session SET last_seen_ms=?2, expires_ms=?3 WHERE token_hash=?1",
            rusqlite::params![token_hash, now, expires_ms],
        )?;
        Ok(())
    }

    pub fn auth_session_delete(&self, token_hash: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM auth_session WHERE token_hash=?1", [token_hash])?;
        Ok(())
    }

    pub fn auth_sessions(&self) -> Result<Vec<AuthSessionRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM auth_session ORDER BY created_ms DESC")?;
        let rows = stmt
            .query_map([], AuthSessionRow::from_row)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    pub fn auth_sessions_purge(&self, now: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM auth_session WHERE expires_ms <= ?1", [now])?;
        conn.execute(
            "DELETE FROM push_subscription WHERE session_hash IS NOT NULL
             AND session_hash NOT IN (SELECT token_hash FROM auth_session)",
            [],
        )?;
        Ok(())
    }

    // ---- push subscriptions ----

    /// Register or refresh a device by its endpoint: a browser that
    /// resubscribes after a re-login re-binds the same endpoint to the new
    /// session rather than leaving an orphan.
    pub fn push_subscription_upsert(&self, r: &PushSubscriptionRow) -> Result<String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO push_subscription
                (id, session_hash, endpoint, p256dh, auth, user_agent, created_ms, last_ok_ms, fail_count)
             VALUES (?1,?2,?3,?4,?5,?6,?7,NULL,0)
             ON CONFLICT(endpoint) DO UPDATE SET
                session_hash=excluded.session_hash, p256dh=excluded.p256dh,
                auth=excluded.auth, user_agent=excluded.user_agent, fail_count=0",
            rusqlite::params![
                r.id,
                r.session_hash,
                r.endpoint,
                r.p256dh,
                r.auth,
                r.user_agent,
                r.created_ms
            ],
        )?;
        conn.query_row(
            "SELECT id FROM push_subscription WHERE endpoint=?1",
            [&r.endpoint],
            |row| row.get(0),
        )
        .map_err(Into::into)
    }

    /// Every device whose session is still logged in, or that belongs to a
    /// browser on this machine.
    pub fn push_subscriptions_live(&self, now: i64) -> Result<Vec<PushSubscriptionRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT p.* FROM push_subscription p
             LEFT JOIN auth_session a ON a.token_hash = p.session_hash
             WHERE p.session_hash IS NULL OR a.expires_ms > ?1
             ORDER BY p.created_ms",
        )?;
        let rows = stmt.query_map([now], PushSubscriptionRow::from_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn push_subscriptions(&self) -> Result<Vec<PushSubscriptionRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM push_subscription ORDER BY created_ms")?;
        let rows = stmt.query_map([], PushSubscriptionRow::from_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn push_subscription_delete(&self, id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM push_subscription WHERE id=?1", [id])? > 0)
    }

    pub fn push_subscription_delete_endpoint(&self, endpoint: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "DELETE FROM push_subscription WHERE endpoint=?1",
            [endpoint],
        )? > 0)
    }

    /// A delivery landed: the device is alive, and any run of failures ends.
    pub fn push_subscription_ok(&self, id: &str, now: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE push_subscription SET last_ok_ms=?2, fail_count=0 WHERE id=?1",
            rusqlite::params![id, now],
        )?;
        Ok(())
    }

    /// A delivery did not land; returns the run of failures so far.
    pub fn push_subscription_failed(&self, id: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE push_subscription SET fail_count=fail_count+1 WHERE id=?1",
            [id],
        )?;
        conn.query_row(
            "SELECT fail_count FROM push_subscription WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .map_err(Into::into)
    }

    // ---- review ----

    pub fn insert_review(&self, r: &ReviewRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO review (id, session_id, node_id, channel, kind, title, body, edited_title,
                edited_body, provider, target, diff, files, head_sha, base_ref, added, removed,
                state, verdict_reason, publish_result, claimed_ms, created_ms, created_mono_ms,
                resolved_mono_ms, updated_ms, checks_json, review_session_id, ai_verdict_json, revision_patch)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,
                ?22,?23,?24,?25,?26,?27,?28,?29)",
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
                r.updated_ms,
                r.checks_json,
                r.review_session_id,
                r.ai_verdict_json,
                r.revision_patch
            ],
        )?;
        Ok(())
    }

    pub fn set_checks(&self, id: &str, checks_json: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE review SET checks_json=?2, updated_ms=?3 WHERE id=?1",
            rusqlite::params![id, checks_json, now_ms()],
        )?;
        Ok(())
    }

    /// Attach the review session the node spawned for a review.
    pub fn set_review_session(&self, id: &str, session_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE review SET review_session_id=?2, updated_ms=?3 WHERE id=?1",
            rusqlite::params![id, session_id, now_ms()],
        )?;
        Ok(())
    }

    /// Record a review session's verdict. Never a decision: the human's
    /// verdict is the only one that resolves the row.
    pub fn set_ai_verdict(&self, id: &str, verdict_json: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE review SET ai_verdict_json=?2, updated_ms=?3 WHERE id=?1",
            rusqlite::params![id, verdict_json, now_ms()],
        )?;
        Ok(n == 1)
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
    pub fn request_changes(&self, id: &str, notes: &str, patch: Option<&str>) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE review SET state='revising', verdict_reason=?2, revision_patch=?4, updated_ms=?3
             WHERE id=?1 AND state IN ('new','claimed')",
            rusqlite::params![id, notes, now_ms(), patch],
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
            // The patch is cleared with the notes: it described the diff that
            // has just been replaced.
            "UPDATE review SET diff=?2, files=?3, head_sha=?4, added=?5, removed=?6, state='new',
                verdict_reason=NULL, revision_patch=NULL, claimed_ms=NULL, resolved_mono_ms=NULL,
                updated_ms=?7
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
            providers_json: None,
        };
        store.put_node(&n).unwrap();
        n.id
    }

    #[test]
    fn a_nodes_provider_summary_round_trips_the_wire_shape() {
        let store = Store::open_in_memory().unwrap();
        let id = node(&store);
        let summary = r#"[{"name":"anthropic","state":"connected"}]"#;
        store.set_node_providers(&id, summary).unwrap();
        let v = store.get_node(&id).unwrap().unwrap().to_json();
        assert_eq!(v["providers"][0]["name"], "anthropic");
        // A peer reads the row off the wire: the summary survives.
        let back = NodeRow::from_json(&v).unwrap();
        assert_eq!(back.providers_json.as_deref(), Some(summary));
        // A row from an older build has no providers field at all.
        let mut old = v.clone();
        old.as_object_mut().unwrap().remove("providers");
        assert!(NodeRow::from_json(&old).unwrap().providers_json.is_none());
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
                project_id: None,
                phase: "execute".into(),
                policy_version: None,
                review_id: None,
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
                archived_ms: None,
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
                providers_json: None,
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
