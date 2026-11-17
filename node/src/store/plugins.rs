//! Plugins this node installed (`crate::plugin`): the build that is active,
//! the one before it for rollback, and one waiting on its install card.
//! Per node, never replicated: what a node runs is its own.

use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use super::{now_ms, Result, Store, StoreError};
use crate::plugin::manifest::Manifest;

/// Its first install waits on its card; nothing of it is active.
pub const PENDING: &str = "pending";
pub const ACTIVE: &str = "active";
/// Removed with its data kept; its builds are gone.
pub const REMOVED: &str = "removed";

/// One staged build of a plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Build {
    pub version: String,
    /// The archive's sha256, as published in `checksums.txt` (or the local
    /// archive's own).
    pub digest: String,
    /// The unpacked tree's digest (`crate::plugin::install::tree_digest`),
    /// checked again before the build is activated or run.
    pub tree_digest: String,
    pub manifest: Manifest,
}

/// A build waiting on the operator's install card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    #[serde(flatten)]
    pub build: Build,
    pub approval_id: String,
    /// The release it came from, when it came from one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// The release's notes, as the card showed them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changelog: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PluginRow {
    pub name: String,
    /// `owner/repo`, or `local:<file name>` for an archive the operator
    /// handed the node.
    pub repo: String,
    pub state: String,
    pub active: Option<Build>,
    pub previous: Option<Build>,
    pub pending: Option<Pending>,
    pub updated_ms: i64,
}

impl PluginRow {
    fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Self> {
        let json = |column: &str, text: Option<String>| -> rusqlite::Result<_> {
            text.map(|t| serde_json::from_str(&t))
                .transpose()
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        format!("plugin.{column}: {e}").into(),
                    )
                })
        };
        let manifest: Option<Manifest> = json("manifest", r.get("manifest")?)?;
        let active = match (
            r.get::<_, Option<String>>("version")?,
            r.get::<_, Option<String>>("digest")?,
            r.get::<_, Option<String>>("tree_digest")?,
            manifest,
        ) {
            (Some(version), Some(digest), Some(tree_digest), Some(manifest)) => Some(Build {
                version,
                digest,
                tree_digest,
                manifest,
            }),
            _ => None,
        };
        Ok(Self {
            name: r.get("name")?,
            repo: r.get("repo")?,
            state: r.get("state")?,
            active,
            previous: json("previous", r.get("previous")?)?,
            pending: json("pending", r.get("pending")?)?,
            updated_ms: r.get("updated_ms")?,
        })
    }

    /// Whether the archive at `digest` is one this row holds already.
    pub fn holds(&self, digest: &str) -> bool {
        self.active.as_ref().is_some_and(|b| b.digest == digest)
            || self.previous.as_ref().is_some_and(|b| b.digest == digest)
            || self
                .pending
                .as_ref()
                .is_some_and(|p| p.build.digest == digest)
    }
}

fn text<T: Serialize>(value: Option<&T>) -> Result<Option<String>> {
    value
        .map(serde_json::to_string)
        .transpose()
        .map_err(StoreError::from)
}

impl Store {
    pub fn plugin_get(&self, name: &str) -> Result<Option<PluginRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT * FROM plugin WHERE name=?1",
            [name],
            PluginRow::from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn plugins_list(&self) -> Result<Vec<PluginRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM plugin ORDER BY name")?;
        let rows = stmt
            .query_map([], PluginRow::from_row)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// Hold `pending` for `name`'s card, in place of any build that waited
    /// before it. A plugin not installed, or removed, becomes a first
    /// install from `repo`; an installed one keeps its active build, and its
    /// source.
    pub fn plugin_stage_pending(&self, name: &str, repo: &str, pending: &Pending) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let pending = text(Some(pending))?;
        conn.execute(
            "INSERT INTO plugin (name, repo, state, pending, updated_ms)
             VALUES (?1, ?2, 'pending', ?3, ?4)
             ON CONFLICT(name) DO UPDATE SET
                pending = excluded.pending,
                updated_ms = excluded.updated_ms,
                repo = CASE WHEN plugin.state = 'active' THEN plugin.repo ELSE excluded.repo END,
                state = CASE WHEN plugin.state = 'active' THEN 'active' ELSE 'pending' END",
            rusqlite::params![name, repo, pending, now_ms()],
        )?;
        Ok(())
    }

    /// Make the build waiting on `approval_id` the active one, the active
    /// one becoming the previous. `None` when nothing waits on that card.
    pub fn plugin_activate_pending(&self, name: &str, approval_id: &str) -> Result<Option<Build>> {
        let Some(row) = self.plugin_get(name)? else {
            return Ok(None);
        };
        let Some(pending) = row.pending.filter(|p| p.approval_id == approval_id) else {
            return Ok(None);
        };
        // Reinstalling the active build leaves the previous one as it was.
        let previous = match &row.active {
            Some(active) if active.digest != pending.build.digest => Some(active),
            _ => row.previous.as_ref(),
        };
        let build = pending.build;
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE plugin SET version=?2, digest=?3, tree_digest=?4, manifest=?5,
                state='active', previous=?6, pending=NULL, updated_ms=?7
             WHERE name=?1 AND json_extract(pending, '$.approval_id') = ?8",
            rusqlite::params![
                name,
                build.version,
                build.digest,
                build.tree_digest,
                serde_json::to_string(&build.manifest)?,
                text(previous)?,
                now_ms(),
                approval_id
            ],
        )?;
        Ok((n == 1).then_some(build))
    }

    /// Drop the build waiting on `approval_id`. A plugin that had nothing
    /// active is forgotten. False when nothing waited on that card.
    pub fn plugin_clear_pending(&self, name: &str, approval_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let deleted = conn.execute(
            "DELETE FROM plugin WHERE name=?1 AND version IS NULL
               AND json_extract(pending, '$.approval_id') = ?2",
            rusqlite::params![name, approval_id],
        )?;
        if deleted == 1 {
            return Ok(true);
        }
        let n = conn.execute(
            "UPDATE plugin SET pending=NULL, updated_ms=?3
             WHERE name=?1 AND json_extract(pending, '$.approval_id') = ?2",
            rusqlite::params![name, approval_id, now_ms()],
        )?;
        Ok(n == 1)
    }

    /// Swap the active build and the previous one. `None` when there is no
    /// previous build to go back to; otherwise `(now active, was active)`.
    pub fn plugin_rollback(&self, name: &str) -> Result<Option<(Build, Build)>> {
        let Some(row) = self.plugin_get(name)? else {
            return Ok(None);
        };
        let (Some(active), Some(previous)) = (row.active, row.previous) else {
            return Ok(None);
        };
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE plugin SET version=?2, digest=?3, tree_digest=?4, manifest=?5,
                previous=?6, updated_ms=?7
             WHERE name=?1 AND digest=?8 AND state='active'",
            rusqlite::params![
                name,
                previous.version,
                previous.digest,
                previous.tree_digest,
                serde_json::to_string(&previous.manifest)?,
                text(Some(&active))?,
                now_ms(),
                active.digest
            ],
        )?;
        Ok((n == 1).then_some((previous, active)))
    }

    /// Remove `name`: its builds and anything pending are forgotten, the
    /// row kept as removed, or deleted with `purge`.
    pub fn plugin_remove(&self, name: &str, purge: bool) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = match purge {
            true => conn.execute("DELETE FROM plugin WHERE name=?1", [name])?,
            false => conn.execute(
                "UPDATE plugin SET version=NULL, digest=NULL, tree_digest=NULL, manifest=NULL,
                    previous=NULL, pending=NULL, state='removed', updated_ms=?2
                 WHERE name=?1",
                rusqlite::params![name, now_ms()],
            )?,
        };
        Ok(n == 1)
    }

    /// Every format an installed or pending plugin declares, with its owner.
    pub fn plugin_formats_owned(&self) -> Result<Vec<(String, String)>> {
        let mut owned = Vec::new();
        for row in self.plugins_list()? {
            let manifests = row
                .active
                .iter()
                .map(|b| &b.manifest)
                .chain(row.pending.iter().map(|p| &p.build.manifest));
            for manifest in manifests {
                for format in &manifest.formats {
                    owned.push((format.clone(), row.name.clone()));
                }
            }
        }
        Ok(owned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(version: &str, digest: &str) -> Build {
        let mut manifest =
            Manifest::parse(crate::plugin::manifest::tests::EXAMPLE).unwrap();
        manifest.version = version.into();
        Build {
            version: version.into(),
            digest: digest.into(),
            tree_digest: format!("t{digest}"),
            manifest,
        }
    }

    fn pending(version: &str, digest: &str, approval: &str) -> Pending {
        Pending {
            build: build(version, digest),
            approval_id: approval.into(),
            tag: Some(format!("v{version}")),
            changelog: None,
        }
    }

    #[test]
    fn a_first_install_waits_then_activates_and_an_update_keeps_the_one_before() {
        let store = Store::open_in_memory().unwrap();
        store
            .plugin_stage_pending("sql-review", "cosmicspork/sql", &pending("0.1.0", "d1", "a1"))
            .unwrap();
        let row = store.plugin_get("sql-review").unwrap().unwrap();
        assert_eq!(row.state, PENDING);
        assert!(row.active.is_none());
        assert!(row.holds("d1"));
        // Only its own card activates it.
        assert!(store
            .plugin_activate_pending("sql-review", "other")
            .unwrap()
            .is_none());
        let active = store
            .plugin_activate_pending("sql-review", "a1")
            .unwrap()
            .unwrap();
        assert_eq!(active.version, "0.1.0");
        let row = store.plugin_get("sql-review").unwrap().unwrap();
        assert_eq!(row.state, ACTIVE);
        assert_eq!(row.active.as_ref().unwrap().digest, "d1");
        assert!(row.pending.is_none());

        // An update waits beside the active build, and its source stays.
        store
            .plugin_stage_pending("sql-review", "elsewhere/sql", &pending("0.2.0", "d2", "a2"))
            .unwrap();
        let row = store.plugin_get("sql-review").unwrap().unwrap();
        assert_eq!(row.state, ACTIVE);
        assert_eq!(row.repo, "cosmicspork/sql");
        assert_eq!(row.active.as_ref().unwrap().version, "0.1.0");
        store.plugin_activate_pending("sql-review", "a2").unwrap();
        let row = store.plugin_get("sql-review").unwrap().unwrap();
        assert_eq!(row.active.as_ref().unwrap().version, "0.2.0");
        assert_eq!(row.previous.as_ref().unwrap().version, "0.1.0");

        // Rollback swaps them, and again.
        let (now, was) = store.plugin_rollback("sql-review").unwrap().unwrap();
        assert_eq!((now.version.as_str(), was.version.as_str()), ("0.1.0", "0.2.0"));
        let row = store.plugin_get("sql-review").unwrap().unwrap();
        assert_eq!(row.active.as_ref().unwrap().digest, "d1");
        assert_eq!(row.previous.as_ref().unwrap().digest, "d2");
    }

    #[test]
    fn a_rejected_first_install_is_forgotten_and_a_rejected_update_leaves_the_active() {
        let store = Store::open_in_memory().unwrap();
        store
            .plugin_stage_pending("a", "o/a", &pending("1.0.0", "d1", "a1"))
            .unwrap();
        assert!(!store.plugin_clear_pending("a", "nope").unwrap());
        assert!(store.plugin_clear_pending("a", "a1").unwrap());
        assert!(store.plugin_get("a").unwrap().is_none());

        store
            .plugin_stage_pending("a", "o/a", &pending("1.0.0", "d1", "a1"))
            .unwrap();
        store.plugin_activate_pending("a", "a1").unwrap();
        assert!(store.plugin_rollback("a").unwrap().is_none(), "no previous");
        store
            .plugin_stage_pending("a", "o/a", &pending("1.1.0", "d2", "a2"))
            .unwrap();
        assert!(store.plugin_clear_pending("a", "a2").unwrap());
        let row = store.plugin_get("a").unwrap().unwrap();
        assert_eq!(row.active.unwrap().version, "1.0.0");
        assert!(row.pending.is_none());
    }

    #[test]
    fn removal_keeps_a_row_until_purged_and_a_re_add_is_a_first_install() {
        let store = Store::open_in_memory().unwrap();
        store
            .plugin_stage_pending("a", "o/a", &pending("1.0.0", "d1", "a1"))
            .unwrap();
        store.plugin_activate_pending("a", "a1").unwrap();
        assert_eq!(
            store.plugin_formats_owned().unwrap(),
            Vec::<(String, String)>::new()
        );
        assert!(store.plugin_remove("a", false).unwrap());
        let row = store.plugin_get("a").unwrap().unwrap();
        assert_eq!(row.state, REMOVED);
        assert!(row.active.is_none() && row.previous.is_none());
        store
            .plugin_stage_pending("a", "local:a.tar.gz", &pending("1.0.0", "d1", "a3"))
            .unwrap();
        let row = store.plugin_get("a").unwrap().unwrap();
        assert_eq!(row.state, PENDING);
        assert_eq!(row.repo, "local:a.tar.gz");
        assert!(store.plugin_remove("a", true).unwrap());
        assert!(store.plugin_get("a").unwrap().is_none());
    }

    #[test]
    fn formats_are_owned_by_active_and_pending_builds() {
        let store = Store::open_in_memory().unwrap();
        let mut p = pending("1.0.0", "d1", "a1");
        p.build.manifest.formats = vec!["jira_wiki".into()];
        store.plugin_stage_pending("jira-review", "o/j", &p).unwrap();
        assert_eq!(
            store.plugin_formats_owned().unwrap(),
            vec![("jira_wiki".to_string(), "jira-review".to_string())]
        );
    }
}
