//! Durable continuity-transfer metadata. The signed package is immutable;
//! delivery and import outcomes are append-only events so a source session is
//! never altered to describe a receiver's later work.

use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use super::{now_ms, Result, Store};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferRow {
    pub id: String,
    pub candidate_id: String,
    pub channel: String,
    pub origin_node: String,
    pub target_node: Option<String>,
    pub payload_sha256: String,
    pub package_json: String,
    pub file_count: i64,
    pub document_count: i64,
    pub memory_count: i64,
    pub handoff_note: String,
    pub created_ms: i64,
}

impl TransferRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            candidate_id: row.get("candidate_id")?,
            channel: row.get("channel")?,
            origin_node: row.get("origin_node")?,
            target_node: row.get("target_node")?,
            payload_sha256: row.get("payload_sha256")?,
            package_json: row.get("package_json")?,
            created_ms: row.get("created_ms")?,
            file_count: row.get("file_count")?,
            document_count: row.get("document_count")?,
            memory_count: row.get("memory_count")?,
            handoff_note: row.get("handoff_note")?,
        })
    }
}

/// Listing deliberately omits `package_json`: a package may be tens of MiB,
/// while an inbox needs only immutable provenance and an explicit transfer id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferSummary {
    pub id: String,
    pub candidate_id: String,
    pub channel: String,
    pub origin_node: String,
    pub target_node: Option<String>,
    pub created_ms: i64,
    pub file_count: i64,
    pub document_count: i64,
    pub memory_count: i64,
    pub handoff_note: String,
}

impl TransferSummary {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            candidate_id: row.get("candidate_id")?,
            channel: row.get("channel")?,
            origin_node: row.get("origin_node")?,
            target_node: row.get("target_node")?,
            created_ms: row.get("created_ms")?,
            file_count: row.get("file_count")?,
            document_count: row.get("document_count")?,
            memory_count: row.get("memory_count")?,
            handoff_note: row.get("handoff_note")?,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferEventRow {
    pub transfer_id: String,
    pub kind: String,
    pub detail: Option<String>,
    pub session_id: Option<String>,
    pub at_ms: i64,
}

impl TransferEventRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            transfer_id: row.get("transfer_id")?,
            kind: row.get("kind")?,
            detail: row.get("detail")?,
            session_id: row.get("session_id")?,
            at_ms: row.get("at_ms")?,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferImportRow {
    pub transfer_id: String,
    pub state: String,
    pub workspace_id: Option<String>,
    pub session_id: Option<String>,
    pub detail: Option<String>,
    pub updated_ms: i64,
}

impl TransferImportRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            transfer_id: row.get("transfer_id")?,
            state: row.get("state")?,
            workspace_id: row.get("workspace_id")?,
            session_id: row.get("session_id")?,
            detail: row.get("detail")?,
            updated_ms: row.get("updated_ms")?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportReservation {
    Reserved,
    Preparing,
    Imported,
}

impl Store {
    /// Insert exactly one copy of the signed package. A repeated mesh frame is
    /// deduplicated by its digest, not allowed to overwrite provenance.
    pub fn insert_transfer(&self, transfer: &TransferRow) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "INSERT OR IGNORE INTO transfer
                (id, candidate_id, channel, origin_node, target_node, payload_sha256, package_json,
                 file_count, document_count, memory_count, handoff_note, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                transfer.id,
                transfer.candidate_id,
                transfer.channel,
                transfer.origin_node,
                transfer.target_node,
                transfer.payload_sha256,
                transfer.package_json,
                transfer.file_count,
                transfer.document_count,
                transfer.memory_count,
                transfer.handoff_note,
                transfer.created_ms,
            ],
        )? > 0)
    }

    pub fn transfer(&self, id: &str) -> Result<Option<TransferRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT * FROM transfer WHERE id = ?1",
            [id],
            TransferRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn transfer_summary(&self, id: &str) -> Result<Option<TransferSummary>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, candidate_id, channel, origin_node, target_node, created_ms,
                    file_count, document_count, memory_count, handoff_note
             FROM transfer WHERE id = ?1",
            [id],
            TransferSummary::from_row,
        )
        .optional()
        .map_err(Into::into)
    }
    pub fn transfer_summaries(&self) -> Result<Vec<TransferSummary>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, candidate_id, channel, origin_node, target_node, created_ms,
                    file_count, document_count, memory_count, handoff_note
             FROM transfer ORDER BY created_ms DESC, id DESC LIMIT 200",
        )?;
        let rows = stmt
            .query_map([], TransferSummary::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Atomically reserve a package's single materialization. A survivor of a
    /// crash stays `preparing`: another request must surface uncertainty rather
    /// than create a second continuation session.
    pub fn reserve_transfer_import(&self, id: &str) -> Result<ImportReservation> {
        let conn = self.conn.lock().unwrap();
        if conn.execute(
            "INSERT OR IGNORE INTO transfer_import (transfer_id, state, updated_ms)
             VALUES (?1, 'preparing', ?2)",
            rusqlite::params![id, now_ms()],
        )? > 0
        {
            return Ok(ImportReservation::Reserved);
        }
        let state: String = conn.query_row(
            "SELECT state FROM transfer_import WHERE transfer_id = ?1",
            [id],
            |row| row.get(0),
        )?;
        match state.as_str() {
            "failed" => {
                conn.execute(
                    "UPDATE transfer_import SET state = 'preparing', detail = NULL, updated_ms = ?2
                     WHERE transfer_id = ?1 AND state = 'failed'",
                    rusqlite::params![id, now_ms()],
                )?;
                Ok(ImportReservation::Reserved)
            }
            "imported" => Ok(ImportReservation::Imported),
            _ => Ok(ImportReservation::Preparing),
        }
    }

    pub fn transfer_import(&self, id: &str) -> Result<Option<TransferImportRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT transfer_id, state, workspace_id, session_id, detail, updated_ms
             FROM transfer_import WHERE transfer_id = ?1",
            [id],
            TransferImportRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn complete_transfer_import(
        &self,
        id: &str,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE transfer_import
             SET state = 'imported', workspace_id = ?2, session_id = ?3, detail = NULL, updated_ms = ?4
             WHERE transfer_id = ?1 AND state = 'preparing'",
            rusqlite::params![id, workspace_id, session_id, now_ms()],
        )?;
        Ok(())
    }

    pub fn fail_transfer_import(
        &self,
        id: &str,
        detail: &str,
        workspace_id: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE transfer_import
             SET state = 'failed', workspace_id = COALESCE(?3, workspace_id), detail = ?2, updated_ms = ?4
             WHERE transfer_id = ?1 AND state = 'preparing'",
            rusqlite::params![id, detail, workspace_id, now_ms()],
        )?;
        Ok(())
    }

    pub fn transfer_events(&self, id: &str) -> Result<Vec<TransferEventRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT transfer_id, kind, detail, session_id, at_ms FROM transfer_event
             WHERE transfer_id = ?1 ORDER BY seq",
        )?;
        let rows = stmt
            .query_map([id], TransferEventRow::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn append_transfer_event(
        &self,
        id: &str,
        kind: &str,
        detail: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO transfer_event (transfer_id, kind, detail, session_id, at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, kind, detail, session_id, now_ms()],
        )?;
        Ok(())
    }
}
