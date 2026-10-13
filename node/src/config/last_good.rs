//! The last `node.toml` this node ran well on, and putting it back.
//!
//! A restart applies whatever the file says, and the file can now be edited
//! from elsewhere. An edit that loads but keeps the node from running (a port
//! already taken, a listener it cannot bind, a runtime it cannot reach) would
//! otherwise leave a remote operator with no node to fix it through. So:
//!
//! - Every `serve` counts itself in a small file under the state directory
//!   (`boot-attempts.json`) before it loads the configuration, against the
//!   file's hash. A clean stop, or [`HEALTHY_AFTER`] of running, clears the
//!   count. A start that dies leaves it.
//! - A node that has run [`HEALTHY_AFTER`] copies the file it started from to
//!   `node.toml.last-good`, beside it and with its permissions.
//! - A start that finds [`FAILED_BOOTS`] failed starts in a row of a file that
//!   differs from `node.toml.last-good` sets the failed file aside as
//!   `node.toml.failed-<ms>` and puts last-good in its place. The next
//!   successful start records it in the configuration audit log and tells the
//!   operator's devices.
//!
//! It cannot loop: once restored, the file *is* last-good, and a file equal to
//! last-good is never replaced, however often it fails. Nothing is lost: the
//! file it replaces is kept.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::edit;
use crate::store::{config_audit::ConfigAuditRow, Store};

/// How long a node runs before the file it started from counts as good.
pub const HEALTHY_AFTER: Duration = Duration::from_secs(5 * 60);
/// Failed starts in a row, of one file, before last-good is put back.
pub const FAILED_BOOTS: u32 = 3;
/// Who the audit log says made a restore.
pub const ACTOR: &str = "last-known-good restore";

/// Where the files are.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config: PathBuf,
    pub boots: PathBuf,
}

impl Paths {
    pub fn node() -> Self {
        Self {
            config: super::Config::config_path(),
            boots: super::Config::state_dir().join("boot-attempts.json"),
        }
    }

    pub fn last_good(&self) -> PathBuf {
        sibling(&self.config, ".last-good")
    }

    fn failed(&self, at_ms: i64) -> PathBuf {
        sibling(&self.config, &format!(".failed-{at_ms}"))
    }
}

fn sibling(config: &Path, suffix: &str) -> PathBuf {
    let mut name = config
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| "node.toml".into());
    name.push(suffix);
    config.with_file_name(name)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct Boots {
    /// The file these starts were made with.
    hash: String,
    /// Starts of it that neither stopped cleanly nor ran long enough.
    attempts: u32,
    /// A restore the audit log and the operator have not heard of yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    restored: Option<Restored>,
}

/// One restore.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Restored {
    /// Its audit row's id, made when the restore was, so a start that
    /// records it twice records it once.
    pub id: String,
    pub at_ms: i64,
    pub failed_hash: String,
    pub good_hash: String,
    /// Where the file that failed was kept.
    pub backup: PathBuf,
    pub attempts: u32,
}

fn read_boots(path: &Path) -> Boots {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn write_boots(path: &Path, boots: &Boots) -> std::io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let bytes = serde_json::to_vec_pretty(boots).map_err(std::io::Error::other)?;
    write_atomic(path, &bytes, None)
}

fn read_or_empty(path: &Path) -> std::io::Result<Vec<u8>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// Write `bytes` to `path` whole or not at all, with the permissions of
/// `mode_of` when it exists (else of the file being replaced).
fn write_atomic(path: &Path, bytes: &[u8], mode_of: Option<&Path>) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = sibling(path, &format!(".{}.tmp", uuid::Uuid::now_v7()));
    let written = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        let perms = mode_of
            .and_then(|p| std::fs::metadata(p).ok())
            .or_else(|| std::fs::metadata(path).ok())
            .map(|m| m.permissions());
        if let Some(perms) = perms {
            file.set_permissions(perms)?;
        }
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Count this start, before the configuration is loaded, and put last-good
/// back first if the file has failed [`FAILED_BOOTS`] starts in a row and
/// differs from it. Returns the restore made now, if one was.
///
/// Errors are the state directory's: a node that cannot count its starts
/// still starts, without the safety net.
pub fn on_boot(paths: &Paths, now_ms: i64) -> std::io::Result<Option<Restored>> {
    let current = read_or_empty(&paths.config)?;
    let hash = edit::hash(&current);
    let mut boots = read_boots(&paths.boots);
    if boots.hash != hash {
        // A different file: whatever failed before was not this one.
        boots.hash = hash.clone();
        boots.attempts = 0;
    }
    let mut made = None;
    if boots.attempts >= FAILED_BOOTS {
        match read_or_empty(&paths.last_good()) {
            Ok(good) if !good.is_empty() && edit::hash(&good) != hash => {
                let backup = paths.failed(now_ms);
                // Kept first: if the restore fails half way, the operator's
                // file is still there under one name or the other.
                write_atomic(&backup, &current, Some(&paths.config))?;
                write_atomic(&paths.config, &good, None)?;
                let restored = Restored {
                    id: uuid::Uuid::now_v7().to_string(),
                    at_ms: now_ms,
                    failed_hash: hash.clone(),
                    good_hash: edit::hash(&good),
                    backup,
                    attempts: boots.attempts,
                };
                tracing::error!(
                    attempts = boots.attempts,
                    backup = %restored.backup.display(),
                    "node.toml failed to start the node {} times in a row; put node.toml.last-good back",
                    boots.attempts
                );
                boots = Boots {
                    hash: restored.good_hash.clone(),
                    attempts: 0,
                    restored: Some(restored.clone()),
                };
                made = Some(restored);
            }
            Ok(_) => tracing::warn!(
                attempts = boots.attempts,
                "node.toml has failed to start the node {} times in a row; there is no different last-good file to put back",
                boots.attempts
            ),
            Err(error) => tracing::warn!(%error, "could not read node.toml.last-good"),
        }
    }
    boots.attempts += 1;
    write_boots(&paths.boots, &boots)?;
    Ok(made)
}

/// The node stopped cleanly: this start did not fail.
pub fn clean_stop(paths: &Paths) {
    let mut boots = read_boots(&paths.boots);
    if boots.attempts == 0 {
        return;
    }
    boots.attempts = 0;
    if let Err(error) = write_boots(&paths.boots, &boots) {
        tracing::warn!(%error, "could not record a clean stop");
    }
}

/// The node has run long enough on `boot_text` (the file it started from,
/// not whatever the file is now): keep it as last-good, and stop counting
/// this start as a failure.
pub fn promote(paths: &Paths, boot_text: &str) -> std::io::Result<()> {
    // A node running on its defaults has no file to fall back to.
    if boot_text.is_empty() {
        clean_stop(paths);
        return Ok(());
    }
    let last_good = paths.last_good();
    let same = read_or_empty(&last_good)
        .map(|good| good == boot_text.as_bytes())
        .unwrap_or(false);
    if !same {
        write_atomic(&last_good, boot_text.as_bytes(), Some(&paths.config))?;
        tracing::info!(path = %last_good.display(), "node.toml kept as last-known-good");
    }
    clean_stop(paths);
    Ok(())
}

/// A restore not yet recorded, from this start or an earlier one that did
/// not get as far as recording it.
pub fn unrecorded(paths: &Paths) -> Option<Restored> {
    read_boots(&paths.boots).restored
}

/// Record a restore in the audit log, once, and forget it. The caller tells
/// the operator.
pub fn record(paths: &Paths, store: &Store, restored: &Restored) -> std::io::Result<()> {
    let failed = String::from_utf8_lossy(&read_or_empty(&restored.backup)?).into_owned();
    let good = String::from_utf8_lossy(&read_or_empty(&paths.last_good())?).into_owned();
    let changes: Vec<serde_json::Value> = edit::diff(&failed, &good)
        .iter()
        .map(edit::audit_entry)
        .collect();
    let row = ConfigAuditRow {
        id: restored.id.clone(),
        at_ms: restored.at_ms,
        actor: ACTOR.into(),
        old_hash: restored.failed_hash.clone(),
        new_hash: restored.good_hash.clone(),
        changes: serde_json::Value::Array(changes),
        state: "landed".into(),
    };
    let known = store
        .config_audit_recent(200)
        .map(|rows| rows.iter().any(|r| r.id == row.id))
        .unwrap_or(false);
    if !known {
        store
            .config_audit_insert(&row)
            .and_then(|()| store.config_audit_landed(&row.id))
            .map_err(|e| std::io::Error::other(e.to_string()))?;
    }
    let mut boots = read_boots(&paths.boots);
    if boots.restored.as_ref().map(|r| &r.id) == Some(&restored.id) {
        boots.restored = None;
        write_boots(&paths.boots, &boots)?;
    }
    Ok(())
}

/// At a successful start: record any restore and push it to the operator.
pub fn settle(paths: &Paths, store: &Arc<Store>, cfg: &super::Config) {
    let Some(restored) = unrecorded(paths) else {
        return;
    };
    if let Err(error) = record(paths, store, &restored) {
        tracing::warn!(%error, "could not record the last-known-good restore");
        return;
    }
    let body = format!(
        "node.toml failed to start the node {} times, so the last file it ran well on was put back. The failed one is kept as {}.",
        restored.attempts,
        restored.backup.display()
    );
    let (store, cfg) = (store.clone(), cfg.clone());
    tokio::spawn(async move {
        crate::notify::send_operator(
            &store,
            &cfg,
            &restored.id,
            "node.toml restored".into(),
            body,
            "/settings".into(),
            &[],
        )
        .await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(name: &str) -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().join("node.toml"),
            boots: dir.path().join("state").join(format!("{name}.json")),
        };
        (dir, paths)
    }

    fn write(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn three_failed_starts_put_last_good_back_and_keep_the_failed_file() {
        let (_dir, p) = paths("restore");
        write(&p.config, "node_name = \"good\"\n");
        on_boot(&p, 1).unwrap();
        promote(&p, "node_name = \"good\"\n").unwrap();
        assert_eq!(read(&p.last_good()), "node_name = \"good\"\n");

        write(&p.config, "node_name = \"bad\"\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p.config, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        for at in 2..=4 {
            assert_eq!(on_boot(&p, at).unwrap(), None, "start {at}");
        }
        // The fourth start of the same file finds three that failed.
        let restored = on_boot(&p, 5).unwrap().expect("restored");
        assert_eq!(read(&p.config), "node_name = \"good\"\n");
        assert_eq!(
            restored.backup,
            p.config.with_file_name("node.toml.failed-5")
        );
        assert_eq!(read(&restored.backup), "node_name = \"bad\"\n");
        assert_eq!(restored.attempts, 3);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&restored.backup), 0o600);
            assert_eq!(mode(&p.config), 0o600);
        }

        // Recorded once, in the audit log, then forgotten.
        let store = Store::open_in_memory().unwrap();
        assert_eq!(unrecorded(&p), Some(restored.clone()));
        record(&p, &store, &restored).unwrap();
        record(&p, &store, &restored).unwrap();
        let rows = store.config_audit_recent(10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].actor, ACTOR);
        assert_eq!(rows[0].state, "landed");
        assert_eq!(rows[0].changes[0]["key"], "node_name");
        assert_eq!(unrecorded(&p), None);
    }

    #[test]
    fn a_file_equal_to_last_good_is_never_replaced() {
        let (_dir, p) = paths("equal");
        write(&p.config, "node_name = \"good\"\n");
        promote(&p, "node_name = \"good\"\n").unwrap();
        for at in 0..10 {
            assert_eq!(on_boot(&p, at).unwrap(), None);
        }
        assert_eq!(read(&p.config), "node_name = \"good\"\n");
        let backups = std::fs::read_dir(p.config.parent().unwrap())
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains("failed")
            })
            .count();
        assert_eq!(backups, 0);
    }

    #[test]
    fn without_last_good_nothing_is_restored() {
        let (_dir, p) = paths("none");
        write(&p.config, "node_name = \"bad\"\n");
        for at in 0..5 {
            assert_eq!(on_boot(&p, at).unwrap(), None);
        }
        assert_eq!(read(&p.config), "node_name = \"bad\"\n");
    }

    #[test]
    fn a_clean_stop_or_a_changed_file_starts_the_count_again() {
        let (_dir, p) = paths("count");
        write(&p.config, "node_name = \"good\"\n");
        promote(&p, "node_name = \"good\"\n").unwrap();
        write(&p.config, "node_name = \"other\"\n");
        on_boot(&p, 1).unwrap();
        on_boot(&p, 2).unwrap();
        clean_stop(&p);
        on_boot(&p, 3).unwrap();
        on_boot(&p, 4).unwrap();
        // Edited by hand between starts: a new file, a new count.
        write(&p.config, "node_name = \"third\"\n");
        on_boot(&p, 5).unwrap();
        on_boot(&p, 6).unwrap();
        on_boot(&p, 7).unwrap();
        assert!(on_boot(&p, 8).unwrap().is_some());
        assert_eq!(read(&p.config), "node_name = \"good\"\n");
    }

    #[test]
    fn a_restored_file_that_fails_again_is_left_alone() {
        let (_dir, p) = paths("loop");
        write(&p.config, "node_name = \"good\"\n");
        promote(&p, "node_name = \"good\"\n").unwrap();
        write(&p.config, "node_name = \"bad\"\n");
        for at in 0..3 {
            on_boot(&p, at).unwrap();
        }
        assert!(on_boot(&p, 3).unwrap().is_some());
        // last-good itself keeps failing now: never replaced, never looped.
        for at in 4..20 {
            assert_eq!(on_boot(&p, at).unwrap(), None);
        }
        assert_eq!(read(&p.config), "node_name = \"good\"\n");
    }
}
