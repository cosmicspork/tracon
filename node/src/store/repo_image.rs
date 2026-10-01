use rusqlite::{params, OptionalExtension, Row};
use serde::Serialize;

use super::{now_ms, Result, Store, StoreError};

/// One build of a repository's image on this node.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RepoImageRow {
    pub id: String,
    pub repo_path: String,
    pub kind: String,
    pub recipe_hash: String,
    pub source_ref: String,
    pub source_commit: String,
    /// `name@sha256:…` once built; empty while building or after a failure.
    pub image: String,
    pub parent_image: String,
    /// `building`, `ready` or `failed`.
    pub status: String,
    pub log_tail: String,
    pub error: String,
    /// Commands the entry names whose first word the built image has no tool
    /// for.
    pub warnings: Vec<String>,
    pub started_ms: i64,
    pub finished_ms: Option<i64>,
}

const COLUMNS: &str = "id, repo_path, kind, recipe_hash, source_ref, source_commit, image, \
     parent_image, status, log_tail, error, warnings_json, started_ms, finished_ms";

fn row(r: &Row) -> rusqlite::Result<RepoImageRow> {
    let warnings: String = r.get(11)?;
    Ok(RepoImageRow {
        id: r.get(0)?,
        repo_path: r.get(1)?,
        kind: r.get(2)?,
        recipe_hash: r.get(3)?,
        source_ref: r.get(4)?,
        source_commit: r.get(5)?,
        image: r.get(6)?,
        parent_image: r.get(7)?,
        status: r.get(8)?,
        log_tail: r.get(9)?,
        error: r.get(10)?,
        warnings: serde_json::from_str(&warnings).unwrap_or_default(),
        started_ms: r.get(12)?,
        finished_ms: r.get(13)?,
    })
}

/// What a build is of: the repository, which of its images, and the recipe.
pub struct RepoImageStart<'a> {
    pub repo_path: &'a str,
    pub kind: &'a str,
    pub recipe_hash: &'a str,
    pub source_ref: &'a str,
    pub source_commit: &'a str,
    pub parent_image: &'a str,
}

impl Store {
    fn repo_image_conn(&self) -> Result<std::sync::MutexGuard<'_, rusqlite::Connection>> {
        self.conn
            .lock()
            .map_err(|_| StoreError::Invalid("store lock poisoned".into()))
    }

    pub fn start_repo_image(&self, start: &RepoImageStart) -> Result<String> {
        let id = uuid::Uuid::now_v7().to_string();
        self.repo_image_conn()?.execute(
            "INSERT INTO repo_image (id, repo_path, kind, recipe_hash, source_ref, source_commit, \
             parent_image, status, started_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'building', ?8)",
            params![
                id,
                start.repo_path,
                start.kind,
                start.recipe_hash,
                start.source_ref,
                start.source_commit,
                start.parent_image,
                now_ms()
            ],
        )?;
        Ok(id)
    }

    pub fn finish_repo_image(
        &self,
        id: &str,
        built: std::result::Result<&str, &str>,
        log_tail: &str,
        warnings: &[String],
    ) -> Result<()> {
        let (status, image, error) = match built {
            Ok(image) => ("ready", image, ""),
            Err(error) => ("failed", "", error),
        };
        self.repo_image_conn()?.execute(
            "UPDATE repo_image SET status = ?2, image = ?3, error = ?4, log_tail = ?5, \
             warnings_json = ?6, finished_ms = ?7 WHERE id = ?1",
            params![
                id,
                status,
                image,
                error,
                log_tail,
                serde_json::to_string(warnings).unwrap_or_else(|_| "[]".into()),
                now_ms()
            ],
        )?;
        Ok(())
    }

    /// The image a repository's work runs in: its newest build that finished.
    /// A later failure does not unseat it; the last image that worked is what
    /// keeps a repository usable while its Dockerfile is broken.
    pub fn ready_repo_image(&self, repo_path: &str, kind: &str) -> Result<Option<RepoImageRow>> {
        Ok(self
            .repo_image_conn()?
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM repo_image WHERE repo_path = ?1 AND kind = ?2 \
                     AND status = 'ready' ORDER BY started_ms DESC, id DESC LIMIT 1"
                ),
                params![repo_path, kind],
                row,
            )
            .optional()?)
    }

    /// The newest build of a repository's image, whatever became of it.
    pub fn latest_repo_image(&self, repo_path: &str, kind: &str) -> Result<Option<RepoImageRow>> {
        Ok(self
            .repo_image_conn()?
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM repo_image WHERE repo_path = ?1 AND kind = ?2 \
                     ORDER BY started_ms DESC, id DESC LIMIT 1"
                ),
                params![repo_path, kind],
                row,
            )
            .optional()?)
    }

    pub fn repo_image(&self, id: &str) -> Result<Option<RepoImageRow>> {
        Ok(self
            .repo_image_conn()?
            .query_row(
                &format!("SELECT {COLUMNS} FROM repo_image WHERE id = ?1"),
                params![id],
                row,
            )
            .optional()?)
    }

    /// Every repository this node has built for, with its newest build.
    pub fn repo_images(&self) -> Result<Vec<RepoImageRow>> {
        let conn = self.repo_image_conn()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM repo_image r WHERE started_ms = (SELECT MAX(started_ms) \
             FROM repo_image WHERE repo_path = r.repo_path AND kind = r.kind) \
             ORDER BY repo_path, kind"
        ))?;
        let rows = stmt.query_map([], row)?.collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// A build this process did not finish is over: a restart leaves no
    /// builder running behind a `building` row.
    pub fn reconcile_repo_images(&self) -> Result<()> {
        self.repo_image_conn()?.execute(
            "UPDATE repo_image SET status = 'failed', error = 'the node restarted during the build', \
             finished_ms = ?1 WHERE status = 'building'",
            params![now_ms()],
        )?;
        Ok(())
    }
}
