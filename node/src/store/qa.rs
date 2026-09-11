//! Immutable QA deployment, browser verification, and prototype records.
//!
//! These rows are intentionally local authoritative evidence rather than corpus
//! changes. A rendered demonstration may travel with a channel, but it must not
//! become proof that an environment was observed by some other node.

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::{Result, Store};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QaDeploymentRow {
    pub id: String,
    pub candidate_id: String,
    pub channel: String,
    pub target_id: String,
    pub build_id: String,
    pub execution_image: String,
    pub origin: String,
    /// An operator-configured identity endpoint returned this exact value.
    /// `None` is deliberately not a guessed identity.
    pub environment_identity: Option<String>,
    /// `fresh`, `unknown`, or `failed`. A fresh browser result must have a
    /// fresh deployment observation before and after the run.
    pub identity_state: String,
    pub observed_ms: i64,
    pub started_ms: i64,
    pub finished_ms: i64,
    pub outcome: String,
    pub detail_json: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserRunRow {
    pub id: String,
    pub deployment_id: String,
    pub candidate_id: String,
    pub channel: String,
    pub target_id: String,
    pub authorized_origins_json: String,
    /// A broker key name only. Never an environment key or secret value.
    pub test_credential: Option<String>,
    pub assertions_json: String,
    pub outcome: String,
    pub environment_before: Option<String>,
    pub environment_after: Option<String>,
    /// `fresh`, `stale`, or `unknown`. The row is immutable; stale is derived
    /// again against the newest target observation when it is read.
    pub evidence_state: String,
    /// Bounded runner stderr/stdout without credential values.
    pub log_tail: String,
    pub started_ms: i64,
    pub finished_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QaAssetRow {
    pub id: String,
    pub browser_run_id: String,
    pub candidate_id: String,
    pub channel: String,
    /// `screenshots`, `browser-log`, or `demonstration`.
    pub kind: String,
    pub document_id: String,
    pub document_hash: String,
    pub slug: String,
    pub created_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrototypeRow {
    pub id: String,
    pub candidate_id: String,
    pub channel: String,
    pub source_revision: String,
    pub source_identity_json: String,
    pub build_image: String,
    pub build_inputs_json: String,
    pub document_id: Option<String>,
    pub document_hash: Option<String>,
    pub slug: String,
    pub entry_path: String,
    pub outcome: String,
    pub detail: String,
    pub created_ms: i64,
    pub finished_ms: i64,
}

impl QaDeploymentRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            candidate_id: row.get("candidate_id")?,
            channel: row.get("channel")?,
            target_id: row.get("target_id")?,
            build_id: row.get("build_id")?,
            execution_image: row.get("execution_image")?,
            origin: row.get("origin")?,
            environment_identity: row.get("environment_identity")?,
            identity_state: row.get("identity_state")?,
            observed_ms: row.get("observed_ms")?,
            started_ms: row.get("started_ms")?,
            finished_ms: row.get("finished_ms")?,
            outcome: row.get("outcome")?,
            detail_json: row.get("detail_json")?,
        })
    }
}

impl BrowserRunRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            deployment_id: row.get("deployment_id")?,
            candidate_id: row.get("candidate_id")?,
            channel: row.get("channel")?,
            target_id: row.get("target_id")?,
            authorized_origins_json: row.get("authorized_origins_json")?,
            test_credential: row.get("test_credential")?,
            assertions_json: row.get("assertions_json")?,
            outcome: row.get("outcome")?,
            environment_before: row.get("environment_before")?,
            environment_after: row.get("environment_after")?,
            evidence_state: row.get("evidence_state")?,
            log_tail: row.get("log_tail")?,
            started_ms: row.get("started_ms")?,
            finished_ms: row.get("finished_ms")?,
        })
    }
}

impl QaAssetRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            browser_run_id: row.get("browser_run_id")?,
            candidate_id: row.get("candidate_id")?,
            channel: row.get("channel")?,
            kind: row.get("kind")?,
            document_id: row.get("document_id")?,
            document_hash: row.get("document_hash")?,
            slug: row.get("slug")?,
            created_ms: row.get("created_ms")?,
        })
    }
}

impl PrototypeRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            candidate_id: row.get("candidate_id")?,
            channel: row.get("channel")?,
            source_revision: row.get("source_revision")?,
            source_identity_json: row.get("source_identity_json")?,
            build_image: row.get("build_image")?,
            build_inputs_json: row.get("build_inputs_json")?,
            document_id: row.get("document_id")?,
            document_hash: row.get("document_hash")?,
            slug: row.get("slug")?,
            entry_path: row.get("entry_path")?,
            outcome: row.get("outcome")?,
            detail: row.get("detail")?,
            created_ms: row.get("created_ms")?,
            finished_ms: row.get("finished_ms")?,
        })
    }
}

impl Store {
    // ---- QA observations -------------------------------------------------

    /// Append one deployment observation. Evidence records never update: a
    /// later target observation invalidates an earlier browser run by identity,
    /// rather than rewriting history to say it observed something else.
    pub fn qa_insert_deployment(&self, row: &QaDeploymentRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO qa_deployment (id, candidate_id, channel, target_id, build_id, execution_image, origin,\
                 environment_identity, identity_state, observed_ms, started_ms, finished_ms, outcome, detail_json)\
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![
                row.id, row.candidate_id, row.channel, row.target_id, row.build_id, row.execution_image,
                row.origin, row.environment_identity, row.identity_state, row.observed_ms, row.started_ms,
                row.finished_ms, row.outcome, row.detail_json,
            ],
        )?;
        Ok(())
    }

    pub fn qa_deployment(&self, id: &str) -> Result<Option<QaDeploymentRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT * FROM qa_deployment WHERE id=?1",
            [id],
            QaDeploymentRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn qa_latest_target_observation(&self, target_id: &str) -> Result<Option<QaDeploymentRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT * FROM qa_deployment WHERE target_id=?1\
             ORDER BY observed_ms DESC, id DESC LIMIT 1",
            [target_id],
            QaDeploymentRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn qa_deployments_for_candidate(&self, candidate_id: &str) -> Result<Vec<QaDeploymentRow>> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(
            "SELECT * FROM qa_deployment WHERE candidate_id=?1 ORDER BY observed_ms DESC, id DESC",
        )?;
        let rows = statement
            .query_map([candidate_id], QaDeploymentRow::from_row)?
            .collect::<std::result::Result<_, _>>();
        rows.map_err(Into::into)
    }

    /// Append one browser run. There is no update path: a later external
    /// deployment observation is compared on read and turns this stale.
    pub fn qa_insert_browser_run(&self, row: &BrowserRunRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO qa_browser_run (id, deployment_id, candidate_id, channel, target_id, authorized_origins_json,\
                 test_credential, assertions_json, outcome, environment_before, environment_after, evidence_state,\
                 log_tail, started_ms, finished_ms)\
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            params![
                row.id, row.deployment_id, row.candidate_id, row.channel, row.target_id,
                row.authorized_origins_json, row.test_credential, row.assertions_json, row.outcome,
                row.environment_before, row.environment_after, row.evidence_state, row.log_tail,
                row.started_ms, row.finished_ms,
            ],
        )?;
        Ok(())
    }

    pub fn qa_browser_run(&self, id: &str) -> Result<Option<BrowserRunRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT * FROM qa_browser_run WHERE id=?1",
            [id],
            BrowserRunRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn qa_browser_runs_for_candidate(&self, candidate_id: &str) -> Result<Vec<BrowserRunRow>> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(
            "SELECT * FROM qa_browser_run WHERE candidate_id=?1 ORDER BY started_ms DESC, id DESC",
        )?;
        let rows = statement
            .query_map([candidate_id], BrowserRunRow::from_row)?
            .collect::<std::result::Result<_, _>>();
        rows.map_err(Into::into)
    }

    pub fn qa_insert_asset(&self, row: &QaAssetRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO qa_asset (id, browser_run_id, candidate_id, channel, kind, document_id, document_hash, slug, created_ms)\
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                row.id, row.browser_run_id, row.candidate_id, row.channel, row.kind, row.document_id,
                row.document_hash, row.slug, row.created_ms,
            ],
        )?;
        Ok(())
    }

    pub fn qa_assets_for_run(&self, browser_run_id: &str) -> Result<Vec<QaAssetRow>> {
        let conn = self.conn.lock().unwrap();
        let mut statement =
            conn.prepare("SELECT * FROM qa_asset WHERE browser_run_id=?1 ORDER BY created_ms, id")?;
        let rows = statement
            .query_map([browser_run_id], QaAssetRow::from_row)?
            .collect::<std::result::Result<_, _>>();
        rows.map_err(Into::into)
    }

    // ---- repository-derived prototypes ----------------------------------

    pub fn prototype_insert(&self, row: &PrototypeRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO prototype (id, candidate_id, channel, source_revision, source_identity_json, build_image,\
                 build_inputs_json, document_id, document_hash, slug, entry_path, outcome, detail, created_ms, finished_ms)\
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            params![
                row.id, row.candidate_id, row.channel, row.source_revision, row.source_identity_json,
                row.build_image, row.build_inputs_json, row.document_id, row.document_hash, row.slug,
                row.entry_path, row.outcome, row.detail, row.created_ms, row.finished_ms,
            ],
        )?;
        Ok(())
    }

    pub fn prototype(&self, id: &str) -> Result<Option<PrototypeRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT * FROM prototype WHERE id=?1",
            [id],
            PrototypeRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn prototypes_for_candidate(&self, candidate_id: &str) -> Result<Vec<PrototypeRow>> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(
            "SELECT * FROM prototype WHERE candidate_id=?1 ORDER BY created_ms DESC, id DESC",
        )?;
        let rows = statement
            .query_map([candidate_id], PrototypeRow::from_row)?
            .collect::<std::result::Result<_, _>>();
        rows.map_err(Into::into)
    }
}
