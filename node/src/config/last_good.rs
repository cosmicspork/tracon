//! The last `node.toml` this node ran well on, and putting it back.
//!
//! A restart applies whatever the file says, and the file can now be edited
//! from elsewhere. An edit that keeps the node from starting (a listener it
//! cannot bind, a runtime it cannot reach, a value a start refuses) would
//! otherwise leave a remote operator with no node to fix it through. So:
//!
//! - `serve` holds an exclusive lock on its state directory ([`claim`])
//!   before anything else. A second `serve` on the same state neither counts
//!   nor restores: it stops, saying another node owns the directory.
//! - Every start counts itself in `boot-attempts.json` under the state
//!   directory, against the file's hash, before it loads the configuration.
//!   Serving for [`STARTED_GRACE`] clears the count, as does a clean stop:
//!   only a start that dies before it is serving counts as a failure.
//! - A node that has run [`HEALTHY_AFTER`] keeps the file it started from as
//!   `node.toml.last-good`, beside it and with its permissions. From then on,
//!   every [`HEALTHY_AFTER`] it keeps the file as it is now, when that loads,
//!   held still since the last look, and differs from what the node runs only
//!   in live keys (which it has been running with): last-good follows the
//!   live edits rather than going stale behind them.
//! - A start that finds [`FAILED_BOOTS`] failed starts in a row of one file
//!   puts last-good back only when the file is to blame: it does not load, or
//!   a key that waits for a restart differs from last-good's. A file that
//!   differs only in live keys did not stop the node starting. The failed
//!   file is kept as `node.toml.failed-<ms>`, last-good takes its place, and
//!   the next successful start records it in the configuration audit log and
//!   tells the operator's devices.
//!
//! It cannot loop: once restored, the file *is* last-good, and a file equal to
//! last-good is never replaced, however often it fails. Nothing is lost: the
//! file it replaces is kept first.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::edit;
use crate::store::{config_audit::ConfigAuditRow, Store};

/// How long a node serves before its start no longer counts as failed.
pub const STARTED_GRACE: Duration = Duration::from_secs(30);
/// How long a node runs before the file it started from counts as good, and
/// how often it looks again.
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

    /// The file itself, through any symlink: what is backed up and replaced.
    fn target(&self) -> PathBuf {
        std::fs::canonicalize(&self.config).unwrap_or_else(|_| self.config.clone())
    }

    pub fn last_good(&self) -> PathBuf {
        sibling(&self.target(), ".last-good")
    }

    fn failed(&self, at_ms: i64) -> PathBuf {
        sibling(&self.target(), &format!(".failed-{at_ms}"))
    }

    fn lock(&self) -> PathBuf {
        self.boots.with_file_name("serve.lock")
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

/// This process's hold on its state directory and its `node.toml`, for as
/// long as it serves.
pub struct StateLock {
    _state: std::fs::File,
    _config: std::fs::File,
}

fn try_lock(path: &Path, held: &str) -> std::io::Result<std::fs::File> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            format!(
                "another node owns this {held} ({}); not starting a second one on it",
                path.display()
            ),
        )),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

/// Take the state directory and the `node.toml` this `serve` runs on: a
/// second node on either (a scratch node pointed at the real config
/// directory, say) would otherwise count its failures against the first
/// one's file. `ErrorKind::WouldBlock` means another node holds one of them.
pub fn claim(paths: &Paths) -> std::io::Result<StateLock> {
    let state = try_lock(&paths.lock(), "state directory")?;
    let config = try_lock(&sibling(&paths.target(), ".serve.lock"), "node.toml")?;
    Ok(StateLock {
        _state: state,
        _config: config,
    })
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct Boots {
    /// The file these starts were made with.
    hash: String,
    /// Starts of it that died before they were serving.
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
/// `mode_of` when it exists (else of the file being replaced, else private).
/// The temporary file is private from the moment it exists.
fn write_atomic(path: &Path, bytes: &[u8], mode_of: Option<&Path>) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = sibling(path, &format!(".{}.tmp", uuid::Uuid::now_v7()));
    let written = (|| {
        let mut open = std::fs::OpenOptions::new();
        open.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open.mode(0o600);
        }
        let mut file = open.open(&tmp)?;
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

/// `text` as a start reads it: the retired harness migrated.
fn migrated(text: &str) -> String {
    match crate::legacy::migrate_config(text) {
        Some(migration) => migration.text,
        None => text.to_string(),
    }
}

/// Why `current` is to blame for failing to start, against `good`; `None`
/// when it is not (it loads, and differs from last-good only in live keys),
/// or when last-good would not load either, so putting it back would not
/// help.
fn to_blame(paths: &Paths, current: &str, good: &str) -> Option<String> {
    if let Err(why) = edit::load_text(good, &paths.config) {
        tracing::warn!(%why, "node.toml.last-good does not load; it is not put back");
        return None;
    }
    if let Err(why) = edit::load_text(current, &paths.config) {
        return Some(format!("it does not load: {why}"));
    }
    let keys = super::live::pending_restart(&migrated(good), &migrated(current));
    (!keys.is_empty()).then(|| format!("it differs from last-good in {}", keys.join(", ")))
}

/// Count this start, before the configuration is loaded, and put last-good
/// back first if the file has failed [`FAILED_BOOTS`] starts in a row and is
/// to blame for it. Returns the restore made now, if one was. The caller
/// holds [`claim`].
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
        let good = read_or_empty(&paths.last_good())?;
        let current_text = String::from_utf8_lossy(&current);
        let good_text = String::from_utf8_lossy(&good);
        let blame = (!good.is_empty() && edit::hash(&good) != hash)
            .then(|| to_blame(paths, &current_text, &good_text))
            .flatten();
        match blame {
            Some(why) => {
                // Under the lock every `node.toml` writer takes, so no edit
                // lands between keeping the failed file and replacing it.
                let _held = edit::lock(&paths.config).map_err(std::io::Error::other)?;
                // And still the file that failed: one edited since is a new
                // file, with a count of its own.
                let again = read_or_empty(&paths.config)?;
                if edit::hash(&again) != hash {
                    tracing::warn!("node.toml changed as it was about to be restored; leaving it");
                    boots = Boots {
                        hash: edit::hash(&again),
                        attempts: 1,
                        restored: boots.restored.take(),
                    };
                    write_boots(&paths.boots, &boots)?;
                    return Ok(None);
                }
                let target = paths.target();
                let backup = paths.failed(now_ms);
                // Kept first: if the restore fails half way, the operator's
                // file is still there under one name or the other.
                write_atomic(&backup, &current, Some(&target))?;
                write_atomic(&target, &good, None)?;
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
                    "node.toml failed to start the node {} times in a row and {why}; put node.toml.last-good back",
                    boots.attempts
                );
                boots = Boots {
                    hash: restored.good_hash.clone(),
                    attempts: 0,
                    restored: Some(restored.clone()),
                };
                made = Some(restored);
            }
            None => tracing::warn!(
                attempts = boots.attempts,
                "the node has failed to start {} times in a row on this node.toml, which is not to blame (or there is no different last-good to put back); leaving it",
                boots.attempts
            ),
        }
    }
    boots.attempts += 1;
    write_boots(&paths.boots, &boots)?;
    Ok(made)
}

/// This start did not fail: it is serving, or it stopped cleanly.
pub fn clean_stop(paths: &Paths) {
    let mut boots = read_boots(&paths.boots);
    if boots.attempts == 0 {
        return;
    }
    boots.attempts = 0;
    if let Err(error) = write_boots(&paths.boots, &boots) {
        tracing::warn!(%error, "could not record that this start did not fail");
    }
}

/// Keep `text` as last-good.
pub fn promote(paths: &Paths, text: &str) -> std::io::Result<()> {
    // A node running on its defaults has no file to fall back to.
    if text.is_empty() {
        return Ok(());
    }
    let last_good = paths.last_good();
    let same = read_or_empty(&last_good)
        .map(|good| good == text.as_bytes())
        .unwrap_or(false);
    if !same {
        write_atomic(&last_good, text.as_bytes(), Some(&paths.target()))?;
        tracing::info!(path = %last_good.display(), "node.toml kept as last-known-good");
    }
    Ok(())
}

/// One of the periodic looks once the node is healthy: keep the file as it
/// is now as last-good if it loads, has not changed since the last look
/// (`previous`, updated here), is the version the running node has taken
/// in (its live keys applied, not refused), and differs from the file the
/// node started from only in live keys. Such a file is one the node has been
/// running with.
pub fn keep_current(paths: &Paths, cfg: &super::Config, previous: &mut Option<String>) {
    let Ok(bytes) = read_or_empty(&paths.config) else {
        return;
    };
    let hash = edit::hash(&bytes);
    let held = previous.as_deref() == Some(hash.as_str());
    *previous = Some(hash.clone());
    if !held || cfg.running_hash().as_deref() != Some(hash.as_str()) {
        return;
    }
    let Some(boot_text) = cfg.boot_text() else {
        return;
    };
    let text = String::from_utf8_lossy(&bytes);
    if edit::load_text(&text, &paths.config).is_err()
        || !super::live::pending_restart(&boot_text, &text).is_empty()
    {
        return;
    }
    if let Err(error) = promote(paths, &text) {
        tracing::warn!(%error, "could not keep node.toml as last-known-good");
    }
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
        // The restore works on the resolved file, so compare resolved
        // spellings: macOS's temp dir sits behind /var -> /private/var.
        assert_eq!(
            restored.backup,
            p.config
                .canonicalize()
                .unwrap()
                .with_file_name("node.toml.failed-5")
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

    #[test]
    fn a_file_differing_only_in_live_keys_is_not_blamed() {
        let (_dir, p) = paths("live-only");
        let good = "node_name = \"good\"\n";
        write(&p.config, good);
        promote(&p, good).unwrap();
        // Only a live key differs: whatever stopped the node, it was not this.
        let edited = "node_name = \"good\"\n[review]\nmax_files = 3\n";
        write(&p.config, edited);
        for at in 0..10 {
            assert_eq!(on_boot(&p, at).unwrap(), None);
        }
        assert_eq!(read(&p.config), edited);
    }

    #[test]
    fn a_file_that_does_not_load_is_blamed() {
        let (_dir, p) = paths("unloadable");
        let good = "node_name = \"good\"\n";
        write(&p.config, good);
        promote(&p, good).unwrap();
        // Parses, but a start refuses it; and only in a key that is live.
        write(
            &p.config,
            "node_name = \"good\"\n[review]\nmax_files = \"many\"\n",
        );
        for at in 0..3 {
            on_boot(&p, at).unwrap();
        }
        assert!(on_boot(&p, 3).unwrap().is_some());
        assert_eq!(read(&p.config), good);
    }

    #[test]
    fn a_second_claim_on_the_state_directory_is_refused() {
        let (_dir, p) = paths("claim");
        let held = claim(&p).unwrap();
        let second = claim(&p).err().expect("refused");
        assert_eq!(second.kind(), std::io::ErrorKind::WouldBlock);
        assert!(second.to_string().contains("another node owns"), "{second}");
        drop(held);
        claim_once_released(&p);
    }

    #[test]
    fn last_good_follows_live_edits_once_they_hold_still() {
        let (_dir, p) = paths("follow");
        let boot = "node_name = \"n\"\n";
        write(&p.config, boot);
        promote(&p, boot).unwrap();
        let cfg = super::super::Config::from_toml(boot, &p.config).unwrap();
        cfg.set_boot_text(boot);
        let mut previous = None;
        // A live edit the node has not taken in yet is not kept.
        let live = "node_name = \"n\"\n[supervision]\nchecks = [\"just check\"]\n";
        write(&p.config, live);
        keep_current(&p, &cfg, &mut previous);
        keep_current(&p, &cfg, &mut previous);
        assert_eq!(read(&p.last_good()), boot);
        // Taken in: kept once two looks agree on it.
        let loaded = super::super::Config::from_toml(live, &p.config).unwrap();
        cfg.apply_live_at(&p.config, &loaded, &edit::hash(live.as_bytes()));
        keep_current(&p, &cfg, &mut previous);
        assert_eq!(read(&p.last_good()), live);
        // A change that waits for a restart is not something the node has
        // run on, however long it sits there.
        let owed = "node_name = \"other\"\n";
        write(&p.config, owed);
        keep_current(&p, &cfg, &mut previous);
        keep_current(&p, &cfg, &mut previous);
        assert_eq!(read(&p.last_good()), live);
    }

    #[test]
    fn a_last_good_that_does_not_load_is_not_put_back() {
        let (_dir, p) = paths("bad-good");
        write(&p.last_good(), "[review]\nmax_files = \"many\"\n");
        write(&p.config, "node_name = \"bad\"\n");
        for at in 0..10 {
            assert_eq!(on_boot(&p, at).unwrap(), None);
        }
        assert_eq!(read(&p.config), "node_name = \"bad\"\n");
    }

    #[test]
    fn a_second_node_on_the_same_node_toml_is_refused_whatever_its_state() {
        let (dir, p) = paths("config-claim");
        let held = claim(&p).unwrap();
        let other = Paths {
            config: p.config.clone(),
            boots: dir.path().join("other-state").join("boots.json"),
        };
        let refused = claim(&other).err().expect("refused");
        assert_eq!(refused.kind(), std::io::ErrorKind::WouldBlock);
        assert!(refused.to_string().contains("node.toml"), "{refused}");
        drop(held);
        claim_once_released(&other);
    }

    /// A child another test thread is forking holds a copy of every fd, the
    /// dropped lock's included, until it execs. Commands with their own `PATH`
    /// are forked rather than spawned, and fork is slow on macOS.
    fn claim_once_released(p: &Paths) -> StateLock {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match claim(p) {
                Ok(lock) => return lock,
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(e) => panic!("{e}"),
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_file_is_restored_through_the_link() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let target = real.join("node.toml");
        let link = dir.path().join("node.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let p = Paths {
            config: link.clone(),
            boots: dir.path().join("state").join("boots.json"),
        };
        write(&target, "node_name = \"good\"\n");
        promote(&p, "node_name = \"good\"\n").unwrap();
        assert!(real.join("node.toml.last-good").exists());
        write(&target, "node_name = \"bad\"\n");
        for at in 0..3 {
            on_boot(&p, at).unwrap();
        }
        let restored = on_boot(&p, 3).unwrap().expect("restored");
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(read(&link), "node_name = \"good\"\n");
        assert_eq!(
            restored.backup,
            real.canonicalize().unwrap().join("node.toml.failed-3")
        );
    }
}
