//! The replicated corpus as this node reads it: documents, memories, and
//! promotion batches, written through the sync layer's change log and read
//! locally always. Ranking for recall lives here too, as SQL over the FTS5
//! indexes the `sync` crate maintains.

use crate::corpus::html::{
    content_hash, media_type, validate_bundle, HtmlBundleError, HtmlFile, CHUNK_BYTES,
};
use crate::mcp::docs::kind_of;
use base64::Engine;
use rusqlite::{params, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracon_sync::work::{Readiness, WorkItem};
use tracon_sync::{Applied, Change, ChangeOp};

use super::vectors::Neighbour;

use super::{now_ms, Result, SessionRow, Store, StoreError};

/// Kinds a memory can be. Directives are human-authored and always injected.
pub const KIND_DIRECTIVE: &str = "directive";
pub const KIND_FACT: &str = "fact";
pub const KIND_LESSON: &str = "lesson";
pub const KIND_EPISODE: &str = "episode";

/// Facts fade: a fact half as old as this counts half as much.
const FACT_HALF_LIFE_MS: f64 = 90.0 * 24.0 * 3600.0 * 1000.0;
/// Below this a fact is proposed rather than active.
pub const CONFIDENT: f64 = 0.7;

fn default_document_format() -> String {
    "markdown".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DocumentRow {
    pub id: String,
    pub channel: String,
    pub slug: String,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub hash: String,
    #[serde(default = "default_document_format")]
    pub format: String,
    #[serde(default)]
    pub entry_path: Option<String>,
    #[serde(default)]
    pub source_name: Option<String>,
    pub site: String,
    pub hlc_ms: i64,
    pub deleted: i64,
    /// Kept and readable by slug, but out of listings and search unless asked.
    #[serde(default)]
    pub archived: i64,
    pub created_ms: i64,
    pub updated_ms: i64,
}

#[derive(Debug)]
pub enum DocumentWrite {
    Written {
        row: Box<DocumentRow>,
        change: Change,
    },
    Conflict {
        hash: String,
        body: String,
    },
    HtmlDocument,
}
#[derive(Debug, thiserror::Error)]
pub enum HtmlWriteError {
    #[error(transparent)]
    Bundle(#[from] HtmlBundleError),
    #[error("source name is invalid")]
    InvalidSourceName,
    #[error("document precondition failed")]
    Conflict { hash: String, body: String },
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Sync(#[from] tracon_sync::SyncError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredHtmlFile {
    pub path: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct HtmlFileMetadata {
    pub path: String,
    pub media_type: String,
    pub size_bytes: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum HtmlReadError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("HTML bundle is incomplete or corrupt: {0}")]
    Corrupt(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryRow {
    pub id: String,
    pub channel: String,
    pub scope: String,
    pub scope_ref: Option<String>,
    pub kind: String,
    pub body: String,
    pub source_session: Option<String>,
    pub source_node: Option<String>,
    pub confidence: f64,
    pub state: String,
    pub site: String,
    pub hlc_ms: i64,
    pub deleted: i64,
    pub created_ms: i64,
    pub updated_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PromotionRow {
    pub id: String,
    pub channel: String,
    pub items_json: String,
    pub state: String,
    pub verdicts_json: Option<String>,
    pub decided_by: Option<String>,
    pub decided_ms: Option<i64>,
    pub site: String,
    pub hlc_ms: i64,
    pub created_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRow {
    pub id: String,
    pub channel: String,
    pub name: String,
    pub remote_url: Option<String>,
    pub created_ms: i64,
}

/// A ledger item as the interface reads it: the row, its derived readiness,
/// and the open session holding it, if any.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkView {
    #[serde(flatten)]
    pub item: WorkItem,
    pub readiness: Readiness,
    pub session_id: Option<String>,
}

pub fn work_from_row(r: &Row) -> rusqlite::Result<WorkItem> {
    let deps: String = r.get("deps_json")?;
    Ok(WorkItem {
        id: r.get("id")?,
        channel: r.get("channel")?,
        project_id: r.get("project_id")?,
        title: r.get("title")?,
        body: r.get("body")?,
        state: r.get("state")?,
        priority: r.get("priority")?,
        deps: serde_json::from_str(&deps).unwrap_or_default(),
        discovered_from: r.get("discovered_from")?,
        discovered_by_session: r.get("discovered_by_session")?,
        phase_plan_slug: r.get("phase_plan_slug")?,
        closed_by_session: r.get("closed_by_session")?,
        created_ms: r.get("created_ms")?,
        updated_ms: r.get("updated_ms")?,
    })
}

/// The columns the sync layer replicates for a work item.
pub fn work_change_row(w: &WorkItem) -> Value {
    json!({
        "channel": w.channel, "project_id": w.project_id, "title": w.title, "body": w.body,
        "state": w.state, "priority": w.priority, "deps_json": serde_json::to_string(&w.deps).unwrap_or_else(|_| "[]".into()),
        "discovered_from": w.discovered_from, "discovered_by_session": w.discovered_by_session,
        "phase_plan_slug": w.phase_plan_slug, "closed_by_session": w.closed_by_session,
        "created_ms": w.created_ms, "updated_ms": w.updated_ms,
    })
}

/// One recall result, across both indexes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecallHit {
    /// `directive`, `fact`, `lesson`, `episode`, or `document`.
    pub kind: String,
    pub id: String,
    /// Documents only: fetch the whole thing by this.
    pub slug: Option<String>,
    pub title: Option<String>,
    /// Documents only: source format for list/search presentation.
    #[serde(default)]
    pub format: Option<String>,
    /// The memory body, or a snippet of the document.
    pub text: String,
    pub scope: Option<String>,
    pub confidence: Option<f64>,
    /// Lower sorts first: the tier, then relevance within it.
    pub rank: f64,
}

impl DocumentRow {
    fn from_row(r: &Row) -> rusqlite::Result<Self> {
        Ok(Self {
            id: r.get("id")?,
            channel: r.get("channel")?,
            slug: r.get("slug")?,
            kind: r.get("kind")?,
            title: r.get("title")?,
            body: r.get("body")?,
            hash: r.get("hash")?,
            format: r.get("format")?,
            entry_path: r.get("entry_path")?,
            source_name: r.get("source_name")?,
            site: r.get("site")?,
            hlc_ms: r.get("hlc_ms")?,
            deleted: r.get("deleted")?,
            archived: r.get("archived")?,
            created_ms: r.get("created_ms")?,
            updated_ms: r.get("updated_ms")?,
        })
    }

    /// The columns the sync layer replicates, as the row JSON it takes.
    pub fn to_change_row(&self) -> Value {
        json!({
            "channel": self.channel, "slug": self.slug, "kind": self.kind, "title": self.title,
            "body": self.body, "hash": self.hash, "archived": self.archived,
            "format": self.format, "entry_path": self.entry_path, "source_name": self.source_name,
            "created_ms": self.created_ms, "updated_ms": self.updated_ms,
        })
    }
}

impl MemoryRow {
    fn from_row(r: &Row) -> rusqlite::Result<Self> {
        Ok(Self {
            id: r.get("id")?,
            channel: r.get("channel")?,
            scope: r.get("scope")?,
            scope_ref: r.get("scope_ref")?,
            kind: r.get("kind")?,
            body: r.get("body")?,
            source_session: r.get("source_session")?,
            source_node: r.get("source_node")?,
            confidence: r.get("confidence")?,
            state: r.get("state")?,
            site: r.get("site")?,
            hlc_ms: r.get("hlc_ms")?,
            deleted: r.get("deleted")?,
            created_ms: r.get("created_ms")?,
            updated_ms: r.get("updated_ms")?,
        })
    }

    pub fn to_change_row(&self) -> Value {
        json!({
            "channel": self.channel, "scope": self.scope, "scope_ref": self.scope_ref, "kind": self.kind,
            "body": self.body, "source_session": self.source_session, "source_node": self.source_node,
            "confidence": self.confidence, "state": self.state, "created_ms": self.created_ms, "updated_ms": self.updated_ms,
        })
    }
}

impl PromotionRow {
    fn from_row(r: &Row) -> rusqlite::Result<Self> {
        Ok(Self {
            id: r.get("id")?,
            channel: r.get("channel")?,
            items_json: r.get("items_json")?,
            state: r.get("state")?,
            verdicts_json: r.get("verdicts_json")?,
            decided_by: r.get("decided_by")?,
            decided_ms: r.get("decided_ms")?,
            site: r.get("site")?,
            hlc_ms: r.get("hlc_ms")?,
            created_ms: r.get("created_ms")?,
        })
    }

    pub fn to_change_row(&self) -> Value {
        json!({
            "channel": self.channel, "items_json": self.items_json, "state": self.state,
            "verdicts_json": self.verdicts_json, "decided_by": self.decided_by, "decided_ms": self.decided_ms,
            "created_ms": self.created_ms,
        })
    }
}

/// How much a vector hit is worth, by its position in the neighbour list.
///
/// Bounded below one tier step on purpose. The tiers are not a relevance
/// heuristic — they are a decision that the operator's standing directives
/// outrank facts, and promoted lessons outrank documents. A semantic match is
/// evidence about relevance, not about that, so it reorders *within* a tier
/// and can never carry a stale fact above a directive.
fn vector_bonus(pos: usize) -> f64 {
    0.9 / (1.0 + pos as f64)
}

/// The best chunk per record, as `id -> (position, offset, len)`. A document
/// matching in three places is one hit at its nearest chunk, not three.
fn nearest_by_source<'a>(
    vectors: &'a [Neighbour],
    table: &str,
) -> std::collections::HashMap<&'a str, (usize, i64, i64)> {
    let mut out = std::collections::HashMap::new();
    for (pos, n) in vectors
        .iter()
        .filter(|n| n.source_table == table)
        .enumerate()
    {
        out.entry(n.source_id.as_str())
            .or_insert((pos, n.offset, n.len));
    }
    out
}

/// What a document is embedded as. The title carries real meaning and is
/// worth indexing — but the corpus's own documents start with the title as an
/// H1, and prepending it again puts it in the snippet twice.
pub fn embed_text(title: &str, body: &str) -> String {
    if title.is_empty() {
        return body.to_string();
    }
    let head = body.trim_start().trim_start_matches('#').trim_start();
    if head.starts_with(title) {
        return body.to_string();
    }
    format!("{title}\n\n{body}")
}

/// The text a vector hit actually matched, read back out of the record. The
/// embedder indexes a document as title then body, so offsets index that.
fn span_of(title: &str, body: &str, offset: i64, len: i64) -> String {
    let text = embed_text(title, body);
    let start = (offset.max(0) as usize).min(text.len());
    let end = ((offset + len).max(0) as usize).min(text.len());
    // Offsets came from this text, but a record edited since indexing may be
    // shorter; falling back to the head is better than panicking on a slice.
    let slice = text.get(start..end).unwrap_or("");
    let slice = if slice.trim().is_empty() {
        &text
    } else {
        slice
    };
    slice.trim().chars().take(400).collect()
}

/// Turn free text into an FTS5 query: each word quoted, any word may match,
/// so punctuation in a question cannot break the syntax.
pub fn fts_query(text: &str) -> String {
    text.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{}\"", w.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

impl Store {
    /// The connection, for the sync crate's planners. Held briefly.
    pub fn conn(&self) -> std::sync::MutexGuard<'_, rusqlite::Connection> {
        self.conn.lock().unwrap()
    }

    /// Channels that hold memories at all, for a node without channel keys.
    pub fn memory_channels(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT DISTINCT channel FROM memory WHERE deleted = 0")?;
        let rows = stmt
            .query_map([], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    // ---- work ledger ----

    pub fn work_get(&self, id: &str) -> Result<Option<WorkItem>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT * FROM work_item WHERE id = ?1 AND deleted = 0",
            [id],
            work_from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Every live item on a channel (optionally one project), unordered.
    pub fn work_list(&self, channel: &str, project_id: Option<&str>) -> Result<Vec<WorkItem>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM work_item WHERE channel = ?1 AND deleted = 0
               AND (?2 IS NULL OR project_id = ?2)",
        )?;
        let rows = stmt
            .query_map(params![channel, project_id], work_from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The ledger as the interface shows it: readiness derived by the sync
    /// crate's order, and the session holding each item. Readiness is
    /// computed over the whole channel so cross-project deps count.
    pub fn work_status(&self, channel: &str, project_id: Option<&str>) -> Result<Vec<WorkView>> {
        let all = self.work_list(channel, None)?;
        let holders = self.work_holders(channel)?;
        Ok(tracon_sync::work::status(&all)
            .into_iter()
            .filter(|(i, _)| project_id.is_none_or(|p| i.project_id.as_deref() == Some(p)))
            .map(|(item, readiness)| {
                let session_id = holders.get(&item.id).cloned();
                WorkView {
                    item,
                    readiness,
                    session_id,
                }
            })
            .collect())
    }

    /// Ready items only, in order, minus those a session already holds.
    pub fn work_ready(&self, channel: &str, project_id: Option<&str>) -> Result<Vec<WorkView>> {
        Ok(self
            .work_status(channel, project_id)?
            .into_iter()
            .filter(|v| v.readiness == Readiness::Ready && v.session_id.is_none())
            .collect())
    }

    /// Item id → the non-terminal session holding it, on any node.
    fn work_holders(&self, channel: &str) -> Result<std::collections::HashMap<String, String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT work_item_id, id FROM session
             WHERE channel = ?1 AND work_item_id IS NOT NULL
               AND state NOT IN ('closed', 'killed_budget', 'failed')
             ORDER BY created_ms",
        )?;
        let rows = stmt.query_map([channel], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out = std::collections::HashMap::new();
        for row in rows {
            let (item, session) = row?;
            out.entry(item).or_insert(session);
        }
        Ok(out)
    }

    /// The non-terminal session holding an item, if any.
    pub fn session_holding(&self, work_item_id: &str) -> Result<Option<SessionRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT * FROM session WHERE work_item_id = ?1
               AND state NOT IN ('closed', 'killed_budget', 'failed')
             ORDER BY created_ms LIMIT 1",
            [work_item_id],
            SessionRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    // ---- sync plumbing ----

    /// Stamp, apply, and log a local write. The caller publishes the change.
    #[allow(clippy::too_many_arguments)]
    pub fn write_change(
        &self,
        site: &str,
        channel: &str,
        table: &str,
        op: ChangeOp,
        id: &str,
        row: Value,
    ) -> Result<Change> {
        let mut conn = self.conn.lock().unwrap();
        tracon_sync::write_change(&mut conn, site, channel, table, op, id, row, now_ms())
            .map_err(sync_err)
    }

    /// Check a document edit precondition and write its replicated change
    /// while holding the store's single writer transaction. `archived` of
    /// `None` keeps whatever the document already was.
    #[allow(clippy::too_many_arguments)]
    pub fn write_document_change(
        &self,
        site: &str,
        channel: &str,
        slug: &str,
        kind: &str,
        title: &str,
        body: &str,
        hash: &str,
        if_hash: Option<&str>,
        create_only: bool,
        new_id: &str,
        archived: Option<bool>,
    ) -> Result<DocumentWrite> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let existing = tx
            .query_row(
                "SELECT * FROM document WHERE channel = ?1 AND slug = ?2 AND deleted = 0
                 ORDER BY hlc_ms DESC, hlc_ctr DESC LIMIT 1",
                params![channel, slug],
                DocumentRow::from_row,
            )
            .optional()?;
        let hash_mismatch = if_hash
            .is_some_and(|want| existing.as_ref().map(|cur| cur.hash.as_str()) != Some(want));
        if (create_only && existing.is_some()) || hash_mismatch {
            let (hash, body) = existing.map(|cur| (cur.hash, cur.body)).unwrap_or_default();
            return Ok(DocumentWrite::Conflict { hash, body });
        }
        if existing.as_ref().is_some_and(|doc| doc.format == "html") {
            return Ok(DocumentWrite::HtmlDocument);
        }

        let now = now_ms();
        let mut row = DocumentRow {
            id: existing
                .as_ref()
                .map(|d| d.id.clone())
                .unwrap_or_else(|| new_id.to_string()),
            channel: channel.to_string(),
            slug: slug.to_string(),
            kind: kind.to_string(),
            title: title.to_string(),
            body: body.to_string(),
            hash: hash.to_string(),
            format: "markdown".to_string(),
            entry_path: None,
            source_name: None,
            site: site.to_string(),
            hlc_ms: 0,
            deleted: 0,
            archived: archived
                .map(i64::from)
                .or(existing.as_ref().map(|d| d.archived))
                .unwrap_or(0),
            created_ms: existing.as_ref().map(|d| d.created_ms).unwrap_or(now),
            updated_ms: now,
        };
        let change = tracon_sync::apply::write_change_in_tx(
            &tx,
            site,
            channel,
            "document",
            ChangeOp::Upsert,
            &row.id,
            row.to_change_row(),
            now,
        )
        .map_err(sync_err)?;
        row.hlc_ms = change.hlc_ms;
        tx.commit()?;
        Ok(DocumentWrite::Written {
            row: Box::new(row),
            change,
        })
    }

    /// Change only an existing document's archive flag, preserving its format,
    /// content identity, and bundle generation.
    pub fn set_document_archived_change(
        &self,
        site: &str,
        channel: &str,
        slug: &str,
        archived: bool,
        if_hash: Option<&str>,
    ) -> Result<DocumentWrite> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let Some(mut row) = tx
            .query_row(
                "SELECT * FROM document WHERE channel = ?1 AND slug = ?2 AND deleted = 0
                 ORDER BY hlc_ms DESC, hlc_ctr DESC LIMIT 1",
                params![channel, slug],
                DocumentRow::from_row,
            )
            .optional()?
        else {
            return Ok(DocumentWrite::Conflict {
                hash: String::new(),
                body: String::new(),
            });
        };
        if if_hash.is_some_and(|want| want != row.hash) {
            return Ok(DocumentWrite::Conflict {
                hash: row.hash,
                body: row.body,
            });
        }
        row.archived = i64::from(archived);
        row.updated_ms = now_ms();
        let change = tracon_sync::apply::write_change_in_tx(
            &tx,
            site,
            channel,
            "document",
            ChangeOp::Upsert,
            &row.id,
            row.to_change_row(),
            row.updated_ms,
        )
        .map_err(sync_err)?;
        row.hlc_ms = change.hlc_ms;
        tx.commit()?;
        Ok(DocumentWrite::Written {
            row: Box::new(row),
            change,
        })
    }

    /// Validate and atomically install one immutable HTML bundle generation.
    ///
    /// Bundle records precede the document change; prior-generation tombstones
    /// follow it. Publishing the returned order prevents peers from observing
    /// a document generation before its files and chunks.
    #[allow(clippy::too_many_arguments)]
    pub fn write_html_document_change(
        &self,
        site: &str,
        channel: &str,
        slug: &str,
        source_name: &str,
        entry_path: &str,
        files: Vec<HtmlFile>,
        if_hash: Option<&str>,
        create_only: bool,
    ) -> std::result::Result<(DocumentRow, Vec<Change>), HtmlWriteError> {
        if source_name.is_empty()
            || source_name.len() > 255
            || source_name.contains(['\0', '/', '\\'])
        {
            return Err(HtmlWriteError::InvalidSourceName);
        }
        let bundle = validate_bundle(slug, entry_path, files)?;
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let existing = tx
            .query_row(
                "SELECT * FROM document WHERE channel = ?1 AND slug = ?2 AND deleted = 0
                 ORDER BY hlc_ms DESC, hlc_ctr DESC LIMIT 1",
                params![channel, slug],
                DocumentRow::from_row,
            )
            .optional()?;
        let hash_mismatch = if_hash
            .is_some_and(|want| existing.as_ref().map(|cur| cur.hash.as_str()) != Some(want));
        if (create_only && existing.is_some()) || hash_mismatch {
            let (hash, body) = existing.map(|cur| (cur.hash, cur.body)).unwrap_or_default();
            return Err(HtmlWriteError::Conflict { hash, body });
        }

        let mut old_chunk_ids: Vec<String> = Vec::new();
        let mut old_file_ids: Vec<String> = Vec::new();
        if let Some(current) = existing.as_ref().filter(|doc| doc.format == "html") {
            let mut stmt = tx.prepare(
                "SELECT id FROM document_bundle_chunk
                 WHERE channel = ?1 AND document_id = ?2 AND document_hash = ?3 AND deleted = 0
                 ORDER BY file_id, chunk_ix",
            )?;
            old_chunk_ids = stmt
                .query_map(params![channel, current.id, current.hash], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<std::result::Result<_, _>>()?;
            let mut stmt = tx.prepare(
                "SELECT id FROM document_bundle_file
                 WHERE channel = ?1 AND document_id = ?2 AND document_hash = ?3 AND deleted = 0
                 ORDER BY path",
            )?;
            old_file_ids = stmt
                .query_map(params![channel, current.id, current.hash], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<std::result::Result<_, _>>()?;
        }

        let now = now_ms();
        let document_id = existing
            .as_ref()
            .map(|doc| doc.id.clone())
            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
        let mut changes = Vec::new();
        for file in &bundle.files {
            let file_id = uuid::Uuid::now_v7().to_string();
            let chunks: Vec<&[u8]> = file.bytes.chunks(CHUNK_BYTES).collect();
            let file_row = json!({
                "channel": channel,
                "document_id": document_id,
                "document_hash": bundle.hash,
                "path": file.path,
                "media_type": media_type(&file.path, file.path == bundle.entry_path),
                "size_bytes": file.bytes.len() as i64,
                "content_hash": content_hash(&file.bytes),
                "chunk_count": chunks.len() as i64,
                "created_ms": now,
                "updated_ms": now,
            });
            changes.push(tracon_sync::apply::write_change_in_tx(
                &tx,
                site,
                channel,
                "document_bundle_file",
                ChangeOp::Upsert,
                &file_id,
                file_row,
                now,
            )?);
            for (chunk_ix, bytes) in chunks.into_iter().enumerate() {
                let chunk_id = uuid::Uuid::now_v7().to_string();
                let chunk_row = json!({
                    "channel": channel,
                    "document_id": document_id,
                    "document_hash": bundle.hash,
                    "file_id": file_id,
                    "chunk_ix": chunk_ix as i64,
                    "bytes_b64": base64::engine::general_purpose::STANDARD.encode(bytes),
                    "content_hash": content_hash(bytes),
                    "created_ms": now,
                    "updated_ms": now,
                });
                changes.push(tracon_sync::apply::write_change_in_tx(
                    &tx,
                    site,
                    channel,
                    "document_bundle_chunk",
                    ChangeOp::Upsert,
                    &chunk_id,
                    chunk_row,
                    now,
                )?);
            }
        }

        let mut row = DocumentRow {
            id: document_id,
            channel: channel.to_string(),
            slug: slug.to_string(),
            kind: kind_of(slug).to_string(),
            title: bundle.title,
            body: bundle.entry_html,
            hash: bundle.hash,
            format: "html".to_string(),
            entry_path: Some(bundle.entry_path),
            source_name: Some(source_name.to_string()),
            site: site.to_string(),
            hlc_ms: 0,
            deleted: 0,
            archived: existing.as_ref().map(|doc| doc.archived).unwrap_or(0),
            created_ms: existing.as_ref().map(|doc| doc.created_ms).unwrap_or(now),
            updated_ms: now,
        };
        let document_change = tracon_sync::apply::write_change_in_tx(
            &tx,
            site,
            channel,
            "document",
            ChangeOp::Upsert,
            &row.id,
            row.to_change_row(),
            now,
        )?;
        row.hlc_ms = document_change.hlc_ms;
        changes.push(document_change);

        for id in old_chunk_ids {
            changes.push(tracon_sync::apply::write_change_in_tx(
                &tx,
                site,
                channel,
                "document_bundle_chunk",
                ChangeOp::Delete,
                &id,
                Value::Null,
                now,
            )?);
        }
        for id in old_file_ids {
            changes.push(tracon_sync::apply::write_change_in_tx(
                &tx,
                site,
                channel,
                "document_bundle_file",
                ChangeOp::Delete,
                &id,
                Value::Null,
                now,
            )?);
        }

        tx.commit()?;
        Ok((row, changes))
    }

    /// Tombstone a document and the current HTML generation in one commit.
    /// Markdown documents still produce exactly one document tombstone.
    pub fn delete_document_change(
        &self,
        site: &str,
        channel: &str,
        slug: &str,
    ) -> Result<Option<Vec<Change>>> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let Some(existing) = tx
            .query_row(
                "SELECT * FROM document WHERE channel = ?1 AND slug = ?2 AND deleted = 0
                 ORDER BY hlc_ms DESC, hlc_ctr DESC LIMIT 1",
                params![channel, slug],
                DocumentRow::from_row,
            )
            .optional()?
        else {
            return Ok(None);
        };
        let mut chunk_ids: Vec<String> = Vec::new();
        let mut file_ids: Vec<String> = Vec::new();
        if existing.format == "html" {
            let mut stmt = tx.prepare(
                "SELECT id FROM document_bundle_chunk
                 WHERE channel = ?1 AND document_id = ?2 AND document_hash = ?3 AND deleted = 0
                 ORDER BY file_id, chunk_ix",
            )?;
            chunk_ids = stmt
                .query_map(params![channel, existing.id, existing.hash], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<std::result::Result<_, _>>()?;
            let mut stmt = tx.prepare(
                "SELECT id FROM document_bundle_file
                 WHERE channel = ?1 AND document_id = ?2 AND document_hash = ?3 AND deleted = 0
                 ORDER BY path",
            )?;
            file_ids = stmt
                .query_map(params![channel, existing.id, existing.hash], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<std::result::Result<_, _>>()?;
        }

        let now = now_ms();
        let mut changes = vec![tracon_sync::apply::write_change_in_tx(
            &tx,
            site,
            channel,
            "document",
            ChangeOp::Delete,
            &existing.id,
            Value::Null,
            now,
        )
        .map_err(sync_err)?];
        for id in chunk_ids {
            changes.push(
                tracon_sync::apply::write_change_in_tx(
                    &tx,
                    site,
                    channel,
                    "document_bundle_chunk",
                    ChangeOp::Delete,
                    &id,
                    Value::Null,
                    now,
                )
                .map_err(sync_err)?,
            );
        }
        for id in file_ids {
            changes.push(
                tracon_sync::apply::write_change_in_tx(
                    &tx,
                    site,
                    channel,
                    "document_bundle_file",
                    ChangeOp::Delete,
                    &id,
                    Value::Null,
                    now,
                )
                .map_err(sync_err)?,
            );
        }
        tx.commit()?;
        Ok(Some(changes))
    }

    pub fn apply_changes(
        &self,
        sender: &str,
        channel: &str,
        changes: &[Change],
    ) -> Result<Vec<Applied>> {
        let mut conn = self.conn.lock().unwrap();
        tracon_sync::apply_changes(&mut conn, sender, channel, changes, now_ms()).map_err(sync_err)
    }

    pub fn changes_of_site_after(
        &self,
        site: &str,
        channel: &str,
        after: i64,
        limit: usize,
    ) -> Result<Vec<Change>> {
        let conn = self.conn.lock().unwrap();
        tracon_sync::apply::changes_of_site_after(&conn, site, channel, after, limit)
            .map_err(sync_err)
    }

    pub fn change_log_max(&self, site: &str, channel: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        tracon_sync::apply::change_log_max(&conn, site, channel).map_err(sync_err)
    }

    pub fn sites_on_channel(&self, channel: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        tracon_sync::apply::sites_on_channel(&conn, channel).map_err(sync_err)
    }

    pub fn prune_tombstones(&self, older_than_ms: i64) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        tracon_sync::apply::prune_tombstones(&conn, older_than_ms).map_err(sync_err)
    }

    // ---- documents ----

    /// The live document at a slug. Two sites creating the same slug offline
    /// converge to two rows; the later write is the one that reads.
    pub fn doc_get(&self, channel: &str, slug: &str) -> Result<Option<DocumentRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT * FROM document WHERE channel = ?1 AND slug = ?2 AND deleted = 0
             ORDER BY hlc_ms DESC, hlc_ctr DESC LIMIT 1",
            params![channel, slug],
            DocumentRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn html_bundle_metadata(&self, document: &DocumentRow) -> Result<Vec<HtmlFileMetadata>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT path, media_type, size_bytes FROM document_bundle_file
             WHERE channel = ?1 AND document_id = ?2 AND document_hash = ?3 AND deleted = 0
             ORDER BY path",
        )?;
        let files = stmt
            .query_map(
                params![document.channel, document.id, document.hash],
                |row| {
                    Ok(HtmlFileMetadata {
                        path: row.get(0)?,
                        media_type: row.get(1)?,
                        size_bytes: row.get(2)?,
                    })
                },
            )?
            .collect::<std::result::Result<_, _>>()?;
        Ok(files)
    }

    /// Reassemble and authenticate the current HTML generation.
    pub fn read_html_bundle(
        &self,
        document: &DocumentRow,
    ) -> std::result::Result<Vec<StoredHtmlFile>, HtmlReadError> {
        use crate::corpus::html::{MAX_BUNDLE_BYTES, MAX_FILES, MAX_FILE_BYTES};

        if document.format != "html" {
            return Err(HtmlReadError::Corrupt("document is not HTML".into()));
        }
        let entry_path = document
            .entry_path
            .as_deref()
            .ok_or_else(|| HtmlReadError::Corrupt("entry path is missing".into()))?;
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, path, media_type, size_bytes, content_hash, chunk_count
             FROM document_bundle_file
             WHERE channel = ?1 AND document_id = ?2 AND document_hash = ?3 AND deleted = 0
             ORDER BY path",
        )?;
        let records: Vec<(String, String, String, i64, String, i64)> = stmt
            .query_map(
                params![document.channel, document.id, document.hash],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )?
            .collect::<std::result::Result<_, _>>()?;
        if records.is_empty() || records.len() > MAX_FILES {
            return Err(HtmlReadError::Corrupt(
                "file set is missing or oversized".into(),
            ));
        }

        let mut total_bytes = 0usize;
        let mut html_files = Vec::with_capacity(records.len());
        let mut media_types = std::collections::HashMap::with_capacity(records.len());
        for (file_id, path, media_type, size_bytes, file_hash, chunk_count) in records {
            if size_bytes < 0 || size_bytes as usize > MAX_FILE_BYTES || chunk_count <= 0 {
                return Err(HtmlReadError::Corrupt(format!(
                    "invalid metadata for {path}"
                )));
            }
            let mut stmt = conn.prepare(
                "SELECT chunk_ix, bytes_b64, content_hash
                 FROM document_bundle_chunk
                 WHERE channel = ?1 AND document_id = ?2 AND document_hash = ?3
                   AND file_id = ?4 AND deleted = 0
                 ORDER BY chunk_ix",
            )?;
            let chunks: Vec<(i64, String, String)> = stmt
                .query_map(
                    params![document.channel, document.id, document.hash, file_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?
                .collect::<std::result::Result<_, _>>()?;
            if chunks.len() != chunk_count as usize {
                return Err(HtmlReadError::Corrupt(format!(
                    "chunk count does not match for {path}"
                )));
            }
            let mut bytes = Vec::with_capacity(size_bytes as usize);
            for (expected_ix, (chunk_ix, encoded, hash)) in chunks.into_iter().enumerate() {
                if chunk_ix != expected_ix as i64 {
                    return Err(HtmlReadError::Corrupt(format!(
                        "chunk order is incomplete for {path}"
                    )));
                }
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(encoded.as_bytes())
                    .map_err(|_| {
                        HtmlReadError::Corrupt(format!("chunk encoding is invalid for {path}"))
                    })?;
                if content_hash(&decoded) != hash {
                    return Err(HtmlReadError::Corrupt(format!(
                        "chunk hash does not match for {path}"
                    )));
                }
                bytes.extend_from_slice(&decoded);
            }
            if bytes.len() != size_bytes as usize || content_hash(&bytes) != file_hash {
                return Err(HtmlReadError::Corrupt(format!(
                    "file hash does not match for {path}"
                )));
            }
            total_bytes = total_bytes
                .checked_add(bytes.len())
                .ok_or_else(|| HtmlReadError::Corrupt("bundle size overflow".into()))?;
            if total_bytes > MAX_BUNDLE_BYTES {
                return Err(HtmlReadError::Corrupt(
                    "bundle exceeds its size limit".into(),
                ));
            }
            media_types.insert(path.clone(), media_type);
            html_files.push(HtmlFile { path, bytes });
        }
        let validated = validate_bundle(&document.slug, entry_path, html_files)
            .map_err(|error| HtmlReadError::Corrupt(error.to_string()))?;
        if validated.hash != document.hash || validated.entry_html != document.body {
            return Err(HtmlReadError::Corrupt(
                "generation hash or entry body does not match".into(),
            ));
        }
        Ok(validated
            .files
            .into_iter()
            .map(|file| StoredHtmlFile {
                media_type: media_types
                    .remove(&file.path)
                    .unwrap_or_else(|| "application/octet-stream".into()),
                path: file.path,
                bytes: file.bytes,
            })
            .collect())
    }

    pub fn doc_by_id(&self, id: &str) -> Result<Option<DocumentRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT * FROM document WHERE id = ?1",
            [id],
            DocumentRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Every live document, optionally on one channel, without bodies.
    /// Archived ones are included, flagged; the caller decides what to show.
    pub fn doc_list(&self, channel: Option<&str>) -> Result<Vec<DocumentRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, channel, slug, kind, title, '' AS body, hash, site, hlc_ms, deleted,
                    archived, format, entry_path, source_name, created_ms, updated_ms
             FROM document WHERE deleted = 0 AND (?1 IS NULL OR channel = ?1)
             ORDER BY channel, kind, slug",
        )?;
        let rows = stmt
            .query_map([channel], DocumentRow::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(dedupe_slugs(rows))
    }

    /// Every live document and memory body on this node, as
    /// `(table, id, channel, body)`, for the embedder to reconcile its index
    /// against. Tombstones are excluded: a deleted record must leave no vector
    /// behind, and `index_record` forgets one whose body has gone.
    pub fn rows_to_embed(&self) -> Result<Vec<(String, String, String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut out = Vec::new();
        let mut stmt = conn.prepare(
            "SELECT id, channel, title, body FROM document
             WHERE deleted = 0 AND format = 'markdown'",
        )?;
        let rows = stmt.query_map([], |r| {
            let title: String = r.get(2)?;
            let body: String = r.get(3)?;
            Ok((
                "document".to_string(),
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                embed_text(&title, &body),
            ))
        })?;
        for row in rows {
            out.push(row?);
        }
        let mut stmt = conn.prepare(
            "SELECT id, channel, body FROM memory
             WHERE deleted = 0 AND state IN ('active', 'promoted')",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                "memory".to_string(),
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Documents matching free text, with a snippet, best first.
    pub fn doc_search(
        &self,
        channel: Option<&str>,
        kind: Option<&str>,
        text: &str,
        limit: usize,
    ) -> Result<Vec<RecallHit>> {
        self.doc_search_hybrid(channel, kind, text, limit, &[])
    }

    /// The same, with the vector index's neighbours folded in. An empty
    /// `vectors` is the text-only path above, unchanged — which is what makes
    /// "a node with no embedding endpoint behaves exactly as before" a
    /// property of the code rather than a claim about it.
    pub fn doc_search_hybrid(
        &self,
        channel: Option<&str>,
        kind: Option<&str>,
        text: &str,
        limit: usize,
        vectors: &[Neighbour],
    ) -> Result<Vec<RecallHit>> {
        let near = nearest_by_source(vectors, "document");
        let q = fts_query(text);
        if q.is_empty() && near.is_empty() {
            return Ok(Vec::new());
        }
        // Scoped: `doc_by_id` below takes the same lock, and this mutex is not
        // reentrant.
        let mut hits: Vec<RecallHit> = {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT d.id, d.slug, d.title, d.format, snippet(document_fts, 1, '', '', '…', 14) AS snip, bm25(document_fts) AS score
                 FROM document_fts JOIN document d ON d.rowid = document_fts.rowid
                 WHERE document_fts MATCH ?1 AND d.deleted = 0 AND d.archived = 0
                   AND (?2 IS NULL OR d.channel = ?2) AND (?3 IS NULL OR d.kind = ?3)
                 ORDER BY score LIMIT ?4",
            )?;
            let rows: Vec<RecallHit> = stmt
                .query_map(params![q, channel, kind, limit as i64], |r| {
                    Ok(RecallHit {
                        kind: "document".into(),
                        id: r.get(0)?,
                        slug: Some(r.get(1)?),
                        title: Some(r.get(2)?),
                        format: Some(r.get(3)?),
                        text: r.get(4)?,
                        scope: None,
                        confidence: None,
                        rank: 3.0 + r.get::<_, f64>(5)?,
                    })
                })?
                .collect::<std::result::Result<_, _>>()?;
            rows
        };
        if near.is_empty() {
            return Ok(hits);
        }
        // A document FTS already found is promoted rather than repeated.
        for h in hits.iter_mut() {
            if let Some((pos, ..)) = near.get(h.id.as_str()) {
                h.rank -= vector_bonus(*pos);
            }
        }
        // And one FTS missed enters at the document tier with no lexical score
        // of its own — which is the entire reason for having vectors.
        let found: std::collections::HashSet<&str> = hits
            .iter()
            .map(|h| h.id.as_str())
            .collect::<Vec<_>>()
            .into_iter()
            .collect();
        let mut fresh = Vec::new();
        for (id, n) in near.iter() {
            if found.contains(id) {
                continue;
            }
            let Some(row) = self.doc_by_id(id)? else {
                continue;
            };
            if row.deleted != 0
                || row.archived != 0
                || channel.is_some_and(|c| c != row.channel)
                || kind.is_some_and(|k| k != row.kind)
            {
                continue;
            }
            fresh.push(RecallHit {
                kind: "document".into(),
                id: row.id.clone(),
                slug: Some(row.slug),
                title: Some(row.title.clone()),
                format: Some(row.format),
                text: span_of(&row.title, &row.body, n.1, n.2),
                scope: None,
                confidence: None,
                rank: 3.0 - vector_bonus(n.0),
            });
        }
        hits.extend(fresh);
        hits.sort_by(|a, b| {
            a.rank
                .partial_cmp(&b.rank)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(limit);
        Ok(hits)
    }

    // ---- memory ----

    pub fn memory_get(&self, id: &str) -> Result<Option<MemoryRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT * FROM memory WHERE id = ?1",
            [id],
            MemoryRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Memories on a channel, newest first, optionally of one state.
    pub fn memory_list(
        &self,
        channel: &str,
        state: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemoryRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM memory WHERE channel = ?1 AND deleted = 0 AND (?2 IS NULL OR state = ?2)
             ORDER BY hlc_ms DESC LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![channel, state, limit as i64], MemoryRow::from_row)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// What a session is always told: the channel's directives, global and for
    /// its project, plus its high-confidence facts. Empty query, no FTS.
    pub fn directives_for(
        &self,
        channel: &str,
        project_id: Option<&str>,
    ) -> Result<Vec<MemoryRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM memory WHERE channel = ?1 AND deleted = 0
               AND state IN ('active', 'promoted')
               AND (scope = 'global' OR scope = 'client' OR (scope = 'project' AND scope_ref = ?2))
               AND (kind = 'directive' OR (kind = 'fact' AND confidence >= ?3))
             ORDER BY CASE kind WHEN 'directive' THEN 0 ELSE 1 END, confidence DESC, created_ms DESC",
        )?;
        let rows = stmt
            .query_map(params![channel, project_id, CONFIDENT], MemoryRow::from_row)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// Ranked recall across memories and documents: directives first, then
    /// facts by confidence and age, then promoted lessons, then documents by
    /// relevance; episodes only when asked for. Scope narrows to what the
    /// session can see: its own, its project's, the client's, the global.
    #[allow(clippy::too_many_arguments)]
    pub fn recall(
        &self,
        channel: &str,
        text: &str,
        project_id: Option<&str>,
        session_id: Option<&str>,
        kinds: Option<&[String]>,
        limit: usize,
    ) -> Result<Vec<RecallHit>> {
        self.recall_hybrid(channel, text, project_id, session_id, kinds, limit, &[])
    }

    /// The same, with the vector index folded in. The tiers survive: they are
    /// a decision about what the operator's own directives are worth, not a
    /// relevance heuristic, and a semantic match is not a reason to rank a
    /// stale fact above a standing instruction.
    #[allow(clippy::too_many_arguments)]
    pub fn recall_hybrid(
        &self,
        channel: &str,
        text: &str,
        project_id: Option<&str>,
        session_id: Option<&str>,
        kinds: Option<&[String]>,
        limit: usize,
        vectors: &[Neighbour],
    ) -> Result<Vec<RecallHit>> {
        let near = nearest_by_source(vectors, "memory");
        let q = fts_query(text);
        if q.is_empty() && near.is_empty() {
            return Ok(Vec::new());
        }
        let want = |k: &str| {
            kinds
                .map(|ks| ks.iter().any(|x| x == k))
                .unwrap_or(k != KIND_EPISODE)
        };
        let now = now_ms() as f64;
        let mut hits: Vec<RecallHit> = Vec::new();
        {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT m.id, m.kind, m.body, m.scope, m.confidence, m.state, m.created_ms, bm25(memory_fts) AS score
                 FROM memory_fts JOIN memory m ON m.rowid = memory_fts.rowid
                 WHERE memory_fts MATCH ?1 AND m.channel = ?2 AND m.deleted = 0
                   AND m.state IN ('active', 'promoted')
                   AND (m.scope = 'global' OR m.scope = 'client'
                        OR (m.scope = 'project' AND m.scope_ref = ?3)
                        OR (m.scope = 'session' AND m.scope_ref = ?4))
                 ORDER BY score LIMIT ?5",
            )?;
            let rows = stmt.query_map(
                params![q, channel, project_id, session_id, (limit * 4) as i64],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, f64>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, i64>(6)?,
                        r.get::<_, f64>(7)?,
                    ))
                },
            )?;
            for row in rows {
                let (id, kind, body, scope, confidence, state, created_ms, score) = row?;
                if !want(&kind) {
                    continue;
                }
                let rank = match kind.as_str() {
                    KIND_DIRECTIVE => score,
                    KIND_FACT => {
                        let age = (now - created_ms as f64).max(0.0);
                        let decay = 0.5f64.powf(age / FACT_HALF_LIFE_MS);
                        1.0 + score - confidence * decay
                    }
                    KIND_LESSON if state == "promoted" => 2.0 + score,
                    KIND_LESSON => continue,
                    _ => 4.0 + score,
                };
                // A memory the vector index also liked is promoted within its
                // tier rather than listed twice.
                let rank = rank
                    - near
                        .get(id.as_str())
                        .map_or(0.0, |(p, ..)| vector_bonus(*p));
                hits.push(RecallHit {
                    kind,
                    id,
                    slug: None,
                    title: None,
                    format: None,
                    text: body,
                    scope: Some(scope),
                    confidence: Some(confidence),
                    rank,
                });
            }
        }
        // Memories FTS missed entirely. They are fetched through the same
        // scope predicate as the search above: a semantic match is never a way
        // to see another project's or another session's memory.
        if !near.is_empty() {
            let seen: std::collections::HashSet<String> =
                hits.iter().map(|h| h.id.clone()).collect();
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT m.id, m.kind, m.body, m.scope, m.confidence, m.state, m.created_ms
                 FROM memory m
                 WHERE m.id = ?1 AND m.channel = ?2 AND m.deleted = 0
                   AND m.state IN ('active', 'promoted')
                   AND (m.scope = 'global' OR m.scope = 'client'
                        OR (m.scope = 'project' AND m.scope_ref = ?3)
                        OR (m.scope = 'session' AND m.scope_ref = ?4))",
            )?;
            for (id, (pos, ..)) in near.iter() {
                if seen.contains(*id) {
                    continue;
                }
                let row = stmt.query_row(params![id, channel, project_id, session_id], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, f64>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, i64>(6)?,
                    ))
                });
                let Ok((id, kind, body, scope, confidence, state, created_ms)) = row else {
                    continue;
                };
                if !want(&kind) || (kind == KIND_LESSON && state != "promoted") {
                    continue;
                }
                // No lexical score to add to, so the tier alone carries it.
                let tier = match kind.as_str() {
                    KIND_DIRECTIVE => 0.0,
                    KIND_FACT => {
                        let age = (now - created_ms as f64).max(0.0);
                        1.0 - confidence * 0.5f64.powf(age / FACT_HALF_LIFE_MS)
                    }
                    KIND_LESSON => 2.0,
                    _ => 4.0,
                };
                hits.push(RecallHit {
                    kind,
                    id,
                    slug: None,
                    title: None,
                    format: None,
                    text: body,
                    scope: Some(scope),
                    confidence: Some(confidence),
                    rank: tier - vector_bonus(*pos),
                });
            }
        }
        if want("document") {
            hits.extend(self.doc_search_hybrid(Some(channel), None, text, limit, vectors)?);
        }
        hits.sort_by(|a, b| {
            a.rank
                .partial_cmp(&b.rank)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(limit);
        Ok(hits)
    }

    // ---- promotions ----

    pub fn promotion_get(&self, id: &str) -> Result<Option<PromotionRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT * FROM promotion WHERE id = ?1",
            [id],
            PromotionRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn open_promotions(&self) -> Result<Vec<PromotionRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM promotion WHERE state = 'open' AND deleted = 0 ORDER BY created_ms ASC",
        )?;
        let rows = stmt
            .query_map([], PromotionRow::from_row)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    // ---- projects ----

    pub fn project_put(&self, p: &ProjectRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO project (id, channel, name, remote_url, created_ms) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, remote_url = excluded.remote_url",
            params![p.id, p.channel, p.name, p.remote_url, p.created_ms],
        )?;
        Ok(())
    }

    pub fn project_get(&self, id: &str) -> Result<Option<ProjectRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT * FROM project WHERE id = ?1", [id], |r| {
            Ok(ProjectRow {
                id: r.get("id")?,
                channel: r.get("channel")?,
                name: r.get("name")?,
                remote_url: r.get("remote_url")?,
                created_ms: r.get("created_ms")?,
            })
        })
        .optional()
        .map_err(Into::into)
    }
}

/// Of rows sharing a slug (two sites created it offline), keep the newest.
fn dedupe_slugs(rows: Vec<DocumentRow>) -> Vec<DocumentRow> {
    let mut out: Vec<DocumentRow> = Vec::with_capacity(rows.len());
    for r in rows {
        if let Some(existing) = out
            .iter_mut()
            .find(|d| d.channel == r.channel && d.slug == r.slug)
        {
            if r.hlc_ms > existing.hlc_ms {
                *existing = r;
            }
        } else {
            out.push(r);
        }
    }
    out
}

fn sync_err(e: tracon_sync::SyncError) -> StoreError {
    match e {
        tracon_sync::SyncError::Sqlite(e) => e.into(),
        tracon_sync::SyncError::Malformed(m) => StoreError::Invalid(m),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(channel: &str, slug: &str, kind: &str, title: &str, body: &str) -> Value {
        json!({"channel": channel, "slug": slug, "kind": kind, "title": title, "body": body, "hash": "h", "created_ms": 1, "updated_ms": 1})
    }
    fn mem(
        kind: &str,
        scope: &str,
        scope_ref: Option<&str>,
        body: &str,
        confidence: f64,
        state: &str,
        created_ms: i64,
    ) -> Value {
        json!({"channel": "personal", "scope": scope, "scope_ref": scope_ref, "kind": kind, "body": body,
               "source_session": null, "source_node": null, "confidence": confidence, "state": state,
               "created_ms": created_ms, "updated_ms": created_ms})
    }

    #[test]
    fn documents_are_read_by_slug_searched_and_deduplicated() {
        let s = Store::open_in_memory().unwrap();
        s.write_change(
            "A",
            "personal",
            "document",
            ChangeOp::Upsert,
            "d1",
            doc(
                "personal",
                "guide-workspace",
                "guide",
                "Workspace",
                "run just test to test",
            ),
        )
        .unwrap();
        s.write_change(
            "A",
            "personal",
            "document",
            ChangeOp::Upsert,
            "d2",
            doc(
                "personal",
                "ref-deploy",
                "ref",
                "Deploy",
                "flux reconciles main",
            ),
        )
        .unwrap();
        assert_eq!(
            s.doc_get("personal", "guide-workspace")
                .unwrap()
                .unwrap()
                .body,
            "run just test to test"
        );
        assert!(s.doc_get("work", "guide-workspace").unwrap().is_none());
        let hits = s
            .doc_search(Some("personal"), None, "how do I test?", 5)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].slug.as_deref(), Some("guide-workspace"));
        assert!(hits[0].text.contains("test"));
        assert_eq!(s.doc_search(None, Some("ref"), "flux", 5).unwrap().len(), 1);
        assert_eq!(s.doc_list(Some("personal")).unwrap().len(), 2);
        // A second site created the same slug later: the list shows one, the newer.
        s.write_change(
            "B",
            "personal",
            "document",
            ChangeOp::Upsert,
            "d3",
            doc("personal", "ref-deploy", "ref", "Deploy (B)", "newer"),
        )
        .unwrap();
        let list = s.doc_list(Some("personal")).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(
            s.doc_get("personal", "ref-deploy").unwrap().unwrap().id,
            "d3"
        );
        // Deleting hides it from every read.
        s.write_change(
            "B",
            "personal",
            "document",
            ChangeOp::Delete,
            "d3",
            Value::Null,
        )
        .unwrap();
        assert_eq!(
            s.doc_get("personal", "ref-deploy").unwrap().unwrap().id,
            "d2"
        );
    }

    #[test]
    fn recall_ranks_directives_then_facts_by_confidence_and_age_then_lessons_then_documents() {
        let s = Store::open_in_memory().unwrap();
        let now = now_ms();
        s.write_change(
            "A",
            "personal",
            "memory",
            ChangeOp::Upsert,
            "m-dir",
            mem(
                KIND_DIRECTIVE,
                "global",
                None,
                "the test command is just test",
                1.0,
                "active",
                now,
            ),
        )
        .unwrap();
        s.write_change(
            "A",
            "personal",
            "memory",
            ChangeOp::Upsert,
            "m-fact-new",
            mem(
                KIND_FACT,
                "project",
                Some("p1"),
                "tests live under node/tests",
                0.9,
                "active",
                now,
            ),
        )
        .unwrap();
        s.write_change(
            "A",
            "personal",
            "memory",
            ChangeOp::Upsert,
            "m-fact-old",
            mem(
                KIND_FACT,
                "project",
                Some("p1"),
                "tests used to be slow",
                0.9,
                "active",
                now - 400 * 24 * 3600 * 1000,
            ),
        )
        .unwrap();
        s.write_change(
            "A",
            "personal",
            "memory",
            ChangeOp::Upsert,
            "m-lesson",
            mem(
                KIND_LESSON,
                "global",
                None,
                "flaky tests hide behind retries",
                0.8,
                "promoted",
                now,
            ),
        )
        .unwrap();
        s.write_change(
            "A",
            "personal",
            "memory",
            ChangeOp::Upsert,
            "m-proposed",
            mem(
                KIND_LESSON,
                "global",
                None,
                "a proposed lesson about tests",
                0.8,
                "proposed",
                now,
            ),
        )
        .unwrap();
        s.write_change(
            "A",
            "personal",
            "memory",
            ChangeOp::Upsert,
            "m-other-project",
            mem(
                KIND_FACT,
                "project",
                Some("p2"),
                "another project's test fact",
                0.9,
                "active",
                now,
            ),
        )
        .unwrap();
        s.write_change(
            "A",
            "personal",
            "memory",
            ChangeOp::Upsert,
            "m-episode",
            mem(
                KIND_EPISODE,
                "session",
                Some("s1"),
                "ran the tests once",
                1.0,
                "active",
                now,
            ),
        )
        .unwrap();
        s.write_change(
            "A",
            "personal",
            "document",
            ChangeOp::Upsert,
            "d1",
            doc(
                "personal",
                "guide-testing",
                "guide",
                "Testing",
                "how the tests are run",
            ),
        )
        .unwrap();

        let hits = s
            .recall("personal", "test", Some("p1"), Some("s1"), None, 10)
            .unwrap();
        let kinds: Vec<&str> = hits.iter().map(|h| h.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["directive", "fact", "fact", "lesson", "document"],
            "{hits:#?}"
        );
        assert_eq!(
            hits[1].id, "m-fact-new",
            "the fresh fact outranks the decayed one"
        );
        assert!(hits
            .iter()
            .all(|h| h.id != "m-proposed" && h.id != "m-other-project"));
        assert!(
            hits.iter().all(|h| h.kind != "episode"),
            "episodes only when asked"
        );
        let eps = s
            .recall(
                "personal",
                "tests",
                None,
                Some("s1"),
                Some(&["episode".to_string()]),
                10,
            )
            .unwrap();
        assert_eq!(eps.len(), 1);
        assert_eq!(eps[0].kind, "episode");

        let always = s.directives_for("personal", Some("p1")).unwrap();
        let ids: Vec<&str> = always.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["m-dir", "m-fact-new", "m-fact-old"]);
        assert!(s
            .recall("personal", "???", None, None, None, 5)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn html_generations_write_bundle_before_document_and_retire_prior_rows() {
        let store = Store::open_in_memory().unwrap();
        let first = vec![
            HtmlFile {
                path: "index.html".into(),
                bytes: b"<title>First</title><script src=\"app.js\"></script>".to_vec(),
            },
            HtmlFile {
                path: "app.js".into(),
                bytes: vec![b'x'; CHUNK_BYTES + 1],
            },
        ];
        let (created, changes) = store
            .write_html_document_change(
                "node-a",
                "personal",
                "ref-demo",
                "demo",
                "index.html",
                first,
                None,
                true,
            )
            .unwrap();
        assert_eq!(created.format, "html");
        assert_eq!(created.title, "First");
        assert_eq!(
            created.body,
            "<title>First</title><script src=\"app.js\"></script>"
        );
        assert!(store
            .rows_to_embed()
            .unwrap()
            .iter()
            .all(|(table, id, ..)| table != "document" || id != &created.id));
        let document_ix = changes
            .iter()
            .position(|change| change.table == "document")
            .unwrap();
        assert!(changes[..document_ix]
            .iter()
            .all(|change| change.op == ChangeOp::Upsert));
        assert!(changes[..document_ix]
            .iter()
            .any(|change| change.table == "document_bundle_chunk"));

        let stale = store.write_html_document_change(
            "node-a",
            "personal",
            "ref-demo",
            "demo",
            "index.html",
            vec![HtmlFile {
                path: "index.html".into(),
                bytes: b"<title>Stale</title>".to_vec(),
            }],
            Some("stale"),
            false,
        );
        assert!(matches!(stale, Err(HtmlWriteError::Conflict { .. })));

        let (replaced, changes) = store
            .write_html_document_change(
                "node-a",
                "personal",
                "ref-demo",
                "demo",
                "index.html",
                vec![HtmlFile {
                    path: "index.html".into(),
                    bytes: b"<title>Second</title>".to_vec(),
                }],
                Some(&created.hash),
                false,
            )
            .unwrap();
        let document_ix = changes
            .iter()
            .position(|change| change.table == "document")
            .unwrap();
        assert!(changes[document_ix + 1..]
            .iter()
            .all(|change| change.op == ChangeOp::Delete));
        let conn = store.conn();
        let retired_files: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM document_bundle_file WHERE document_hash = ?1 AND deleted = 1",
                [&created.hash],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retired_files, 2);
        drop(conn);
        let deleted = store
            .delete_document_change("node-a", "personal", "ref-demo")
            .unwrap()
            .unwrap();
        assert_eq!(deleted[0].table, "document");
        assert_eq!(deleted.len(), 3);
        assert!(store.doc_get("personal", "ref-demo").unwrap().is_none());
        let conn = store.conn();
        let live_generation_rows: i64 = conn
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM document_bundle_file
                     WHERE document_hash = ?1 AND deleted = 0) +
                    (SELECT COUNT(*) FROM document_bundle_chunk
                     WHERE document_hash = ?1 AND deleted = 0)",
                [&replaced.hash],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(live_generation_rows, 0);
    }

    #[test]
    fn legacy_document_json_defaults_to_markdown() {
        let document: DocumentRow = serde_json::from_value(json!({
            "id": "legacy",
            "channel": "personal",
            "slug": "guide-legacy",
            "kind": "guide",
            "title": "Legacy",
            "body": "# Legacy",
            "hash": "hash",
            "site": "old-node",
            "hlc_ms": 1,
            "deleted": 0,
            "archived": 0,
            "created_ms": 1,
            "updated_ms": 1
        }))
        .unwrap();
        assert_eq!(document.format, "markdown");
        assert_eq!(document.entry_path, None);
        assert_eq!(document.source_name, None);
    }

    #[test]
    fn fts_queries_survive_punctuation() {
        assert_eq!(
            fts_query("what's the \"test\" command?"),
            "\"what\" OR \"s\" OR \"the\" OR \"test\" OR \"command\""
        );
        assert_eq!(fts_query("  "), "");
    }
}
