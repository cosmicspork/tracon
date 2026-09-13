//! Backup, migration on a clone, and generation-gated restore for one
//! session's OpenCode state.
//!
//! Upstream has none of this. `opencode export` is a redacted transcript,
//! `opencode db` is a raw SQL shell, and there is no backup or snapshot
//! command at all (`config-state.md` §7.2, §9 row 7). Worse than absent: the
//! `migration` table is an applied-id ledger with no `user_version`, no
//! maximum-known check, no fingerprint and no error, so an older binary
//! opening a newer database proceeds silently and can still write to it. The
//! recent migration names are destructive ones. That makes "never promise
//! transparent downgrade" a rule with teeth rather than a caution.
//!
//! So all three operations are the node's, and each is built around one fact
//! it refuses to guess at:
//!
//! * **Backup** needs there to be no writer. The fence in `materialize` says
//!   whether there is one; `--quiesce` is the only way past it, and it stops
//!   the harness through the supervisor's pause and stop rather than killing
//!   it, because a killed harness is exactly the unquiesced writer the backup
//!   was avoiding. Then WAL is checkpointed and `VACUUM INTO` writes the copy,
//!   which is verified by opening it read-only and running `integrity_check`
//!   before anything calls it a backup.
//!
//! * **Upgrade** never touches the original. The database is copied to a
//!   clone, the target binary is run against the clone so its own migrations
//!   apply, the clone's new generation is read and its integrity checked, and
//!   only then is the clone swapped in. An upgrade that fails leaves the
//!   session exactly as it was, and a target binary this host does not have is
//!   said out loud rather than worked around.
//!
//! * **Restore** is gated on the generation, not on hope. A backup whose
//!   applied-migration ids the restoring runtime does not cover is refused,
//!   because the alternative is upstream's silent-write behaviour on a schema
//!   the binary has never seen. A restore also says what it does not bring
//!   back: the workspace as it is now, and every side effect the session had
//!   outside its own database.
//!
//! Everything here works on the session's scratch *volume* rather than on a
//! container path: the state is runtime-owned, and the only sanctioned way in
//! and out of runtime-owned storage is the backend's volume import and export.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::boundary::Backend;
use crate::config::Config;
use crate::session::materialize;
use crate::session::state::SessionState;
use crate::store::{now_ms, OpenCodeStateRow, Store};

/// The manifest written beside every backup.
pub const MANIFEST_FILE: &str = "backup.json";
/// The verified `VACUUM INTO` copy of the session database.
pub const DB_FILE: &str = "opencode.db";
/// Everything else in the state tree, caches excluded.
pub const TAR_FILE: &str = "state.tar";

/// How long a quiesce waits for the harness to let go of its state.
const QUIESCE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// How long the throwaway upgrade server is given to apply migrations.
const UPGRADE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
/// The username the throwaway server is given, and the one it is asked with.
/// Stated rather than defaulted: the credential sent has to be the credential
/// that was set, or a 401 looks like a server that never started.
const SERVER_USERNAME: &str = "opencode";

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("no session {0}")]
    NotFound(String),
    #[error("{0}")]
    Rejected(String),
    #[error("session {session} is {state}; it holds its own state open. Re-run with --quiesce to stop the harness first (pause, then stop — never a kill), or stop the session yourself")]
    Running { session: String, state: String },
    #[error("the state for session {0} is claimed by a writer; a backup or restore of a live database is not a backup")]
    Claimed(String),
    #[error("{0}")]
    NoBackup(String),
    #[error("{0}")]
    NoBinary(String),
    #[error("integrity check on {what} said {said}, not ok")]
    Integrity { what: String, said: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

type Result<T> = std::result::Result<T, StateError>;

/// Stopping a live session so its state can be copied. Implemented by the
/// session manager, which is the only thing that can reach a supervisor.
///
/// It is a pause followed by a stop, and never a kill: a killed harness leaves
/// the WAL exactly as unquiesced as no quiesce at all would have.
#[async_trait]
pub trait Quiesce: Send + Sync {
    async fn quiesce(&self, session_id: &str) -> std::result::Result<(), String>;
}

/// What a backup preserved, and what it could not.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceCheckpoint {
    pub volume: String,
    /// `HEAD` at the instant the state was copied.
    pub commit: Option<String>,
    /// `HEAD^{tree}` at the same instant.
    pub tree: Option<String>,
    /// A digest over the uncommitted changes, so a later restore can say
    /// whether the workspace has moved since without diffing it.
    pub dirty_digest: Option<String>,
    /// Why there is no checkpoint, when there is none. A workspace that could
    /// not be read is said so rather than recorded as clean.
    pub unavailable: Option<String>,
}

impl WorkspaceCheckpoint {
    fn moved_from(&self, now: &WorkspaceCheckpoint) -> bool {
        if self.unavailable.is_some() || now.unavailable.is_some() {
            // Unreadable on one side and readable on the other is a change, or
            // at least indistinguishable from one, and the safe reading of
            // "unknown" is "moved". Unreadable on both sides for the same
            // reason is not: a session that never had a workspace still does
            // not have one, and there is nothing there to have moved.
            return !(self.commit.is_none()
                && now.commit.is_none()
                && self.unavailable == now.unavailable);
        }
        self.commit != now.commit || self.dirty_digest != now.dirty_digest
    }
}

/// Everything a restore needs to decide whether it may proceed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// The manifest format, so a future reader can refuse an older shape
    /// rather than misread it.
    pub manifest_version: u32,
    pub session_id: String,
    pub harness_id: String,
    /// What the harness reported it was when this state was made.
    pub build_version: String,
    /// What the node running it expected.
    pub build_pinned: String,
    /// Applied migration ids in the copied database, in order.
    pub generation: Vec<String>,
    pub generation_digest: String,
    /// The launch manifest the session ran against, when one is recorded.
    pub manifest_digest: Option<String>,
    pub state_volume: String,
    pub state_path: String,
    pub db_file: String,
    pub db_sha256: String,
    pub db_bytes: u64,
    pub tar_file: Option<String>,
    pub tar_sha256: Option<String>,
    /// Volume-relative prefixes deliberately left out of the archive.
    pub excluded: Vec<String>,
    pub workspace: WorkspaceCheckpoint,
    /// What `PRAGMA integrity_check` said about the copy, read-only.
    pub integrity: String,
    pub taken_ms: i64,
    /// Whether the session had to be stopped for this backup.
    pub quiesced: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackupReport {
    pub path: PathBuf,
    pub manifest: Manifest,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpgradeReport {
    pub session_id: String,
    pub from_version: String,
    pub to_version: String,
    pub from_generation: Vec<String>,
    pub to_generation: Vec<String>,
    pub applied: Vec<String>,
    pub clone_path: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct RestoreReport {
    pub session_id: String,
    pub backup: PathBuf,
    pub build_version: String,
    pub generation_digest: String,
    /// What came back.
    pub preserves: Vec<String>,
    /// What did not, in the operator's words rather than a status code.
    pub loses: Vec<String>,
    /// Whether the workspace has moved since the checkpoint.
    pub workspace_moved: bool,
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

fn safe_id(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Where this session's backups live. Node-owned, never mounted anywhere.
pub fn backups_dir(session_id: &str) -> PathBuf {
    Config::state_dir()
        .join("opencode-state")
        .join(safe_id(session_id))
}

fn work_dir(session_id: &str) -> PathBuf {
    Config::state_dir()
        .join("opencode-state-work")
        .join(safe_id(session_id))
}

/// Every backup this node holds for a session, newest first.
pub fn list_backups(session_id: &str) -> Vec<(PathBuf, Manifest)> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(backups_dir(session_id)) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(manifest) = read_manifest(&path).ok().flatten() {
            found.push((path, manifest));
        }
    }
    found.sort_by_key(|(_, m)| std::cmp::Reverse(m.taken_ms));
    found
}

fn read_manifest(dir: &Path) -> Result<Option<Manifest>> {
    let path = dir.join(MANIFEST_FILE);
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_slice(&std::fs::read(path)?)?))
}

// ---------------------------------------------------------------------------
// The fence, held for the length of an operation
// ---------------------------------------------------------------------------

/// A held fence that gives itself back. Backup and restore both take the same
/// one a launch takes, so neither can run beside a harness and neither can be
/// overtaken by a launch halfway through.
struct Fence(String);

impl Fence {
    fn take(session_id: &str) -> Result<Self> {
        materialize::claim_state(session_id)
            .map_err(|_| StateError::Claimed(session_id.to_string()))?;
        Ok(Self(session_id.to_string()))
    }
}

impl Drop for Fence {
    fn drop(&mut self) {
        materialize::release_state(&self.0);
    }
}

// ---------------------------------------------------------------------------
// Reading a database's identity
// ---------------------------------------------------------------------------

/// The applied migration ids in a database, in order. Upstream's ids are its
/// own timestamped filenames, so their lexical order is their chronological
/// one; sorting makes the digest independent of the order rows happen to sit
/// in. A database with no `migration` table has no generation rather than an
/// empty one, and that is an error here — it is not an OpenCode database.
pub fn read_generation(db: &Path) -> Result<Vec<String>> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare("SELECT id FROM migration ORDER BY id")?;
    let ids = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids)
}

/// Open a copy read-only and ask SQLite whether it is intact. This is the only
/// thing that turns a file into a backup: an unverified copy of a database is
/// a file, not a backup.
pub fn integrity_of(db: &Path) -> Result<String> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let said: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    Ok(said)
}

fn verify(db: &Path, what: &str) -> Result<String> {
    let said = integrity_of(db)?;
    if said != "ok" {
        return Err(StateError::Integrity {
            what: what.to_string(),
            said,
        });
    }
    Ok(said)
}

fn digest_file(path: &Path) -> Result<(String, u64)> {
    let bytes = std::fs::read(path)?;
    let mut hash = Sha256::new();
    hash.update(&bytes);
    Ok((hex::encode(hash.finalize()), bytes.len() as u64))
}

// ---------------------------------------------------------------------------
// Version comparison
// ---------------------------------------------------------------------------

fn parts(version: &str) -> Vec<u64> {
    version
        .trim()
        .split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty())
        .map(|p| p.parse().unwrap_or(0))
        .collect()
}

/// Whether `have` is at least `want`. Versions that carry no digits at all are
/// compared as strings: an unparseable pair is only "at least" when it is the
/// same build, which is the conservative answer for a restore gate.
pub fn at_least(have: &str, want: &str) -> bool {
    let (a, b) = (parts(have), parts(want));
    if a.is_empty() || b.is_empty() {
        return have.trim() == want.trim();
    }
    let width = a.len().max(b.len());
    for i in 0..width {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x > y;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// The workspace checkpoint
// ---------------------------------------------------------------------------

async fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = tokio::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        // The same hardening the review capture uses: no external diff or
        // textconv driver, and nothing inherited from the host's git config.
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .await
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// Where the workspace stood at the instant the state was copied.
///
/// This is deliberately not a copy of the workspace. A backup of the harness
/// database restores what the session *knew*; the workspace is a separate,
/// larger thing with its own export path, and conflating the two would make a
/// restore look like it brought the repository back too. What is recorded is
/// enough to say, later, whether the workspace has moved — and a restore that
/// finds it has says so and asks.
async fn checkpoint(backend: &dyn Backend, workspace_id: &str) -> WorkspaceCheckpoint {
    let volume = crate::workspace::volume_name(workspace_id);
    let snapshot = crate::workspace::snapshot_path(workspace_id);
    let mut point = WorkspaceCheckpoint {
        volume: volume.clone(),
        ..Default::default()
    };
    {
        let lock = crate::workspace::volume_lock(&volume);
        let _guard = lock.lock().await;
        if let Err(error) = backend.export_volume(&volume, &snapshot).await {
            point.unavailable = Some(format!("the workspace volume could not be read: {error}"));
            return point;
        }
    }
    let Some(commit) = git(&snapshot, &["rev-parse", "HEAD"]).await else {
        point.unavailable = Some("the workspace has no resolvable HEAD".into());
        return point;
    };
    point.tree = git(&snapshot, &["rev-parse", "HEAD^{tree}"]).await;
    let status = git(&snapshot, &["status", "--porcelain"]).await;
    let diff = git(
        &snapshot,
        &["diff", "--no-ext-diff", "--no-textconv", "HEAD"],
    )
    .await;
    match (status, diff) {
        (Some(status), Some(diff)) => {
            let mut hash = Sha256::new();
            hash.update(status.as_bytes());
            hash.update(b"\0");
            hash.update(diff.as_bytes());
            point.dirty_digest = Some(hex::encode(hash.finalize()));
        }
        _ => {
            point.unavailable = Some("the workspace's uncommitted changes could not be read".into())
        }
    }
    point.commit = Some(commit);
    point
}

fn workspace_id_of(session: &crate::store::SessionRow) -> String {
    session
        .repo_path
        .strip_prefix("workspace://")
        .unwrap_or(&session.id)
        .to_string()
}

// ---------------------------------------------------------------------------
// Backup
// ---------------------------------------------------------------------------

/// Quiesce if allowed, copy, verify, and write down what the copy is.
pub async fn backup(
    store: &Store,
    backend: &dyn Backend,
    quiesce: Option<&dyn Quiesce>,
    session_id: &str,
) -> Result<BackupReport> {
    let session = store
        .get_session(session_id)?
        .ok_or_else(|| StateError::NotFound(session_id.to_string()))?;
    if session.harness_id != crate::adapter::opencode::OpenCodeAdapter::ID {
        return Err(StateError::Rejected(format!(
            "session {session_id} runs {}, which keeps no OpenCode state",
            session.harness_id
        )));
    }

    let live = !SessionState::from_stored(&session.state).is_terminal();
    let quiesced = if live || materialize::state_claimed(session_id) {
        let Some(quiesce) = quiesce else {
            return Err(StateError::Running {
                session: session_id.to_string(),
                state: session.state.clone(),
            });
        };
        quiesce
            .quiesce(session_id)
            .await
            .map_err(StateError::Rejected)?;
        wait_for_release(session_id).await?;
        true
    } else {
        false
    };

    // From here the state is nobody's but this operation's: a launch racing
    // the copy would be the second writer the fence exists to refuse.
    let _fence = Fence::take(session_id)?;

    let volume = crate::workspace::scratch_volume_name(session_id);
    let staging = work_dir(session_id).join("export");
    let _ = std::fs::remove_dir_all(&staging);
    {
        let lock = crate::workspace::volume_lock(&volume);
        let _guard = lock.lock().await;
        backend
            .export_volume(&volume, &staging)
            .await
            .map_err(|e| {
                StateError::Rejected(format!("the session's state could not be read: {e}"))
            })?;
    }

    let source_db = staging.join(materialize::OPENCODE_DB);
    if !source_db.is_file() {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(StateError::Rejected(format!(
            "session {session_id} has no OpenCode database at {}; it may never have started",
            materialize::OPENCODE_DB
        )));
    }

    let taken_ms = now_ms();
    let out = backups_dir(session_id).join(format!("{taken_ms}"));
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out)?;

    // WAL first. `VACUUM INTO` reads through a connection and would otherwise
    // leave the copy's content depending on a `-wal` file the copy does not
    // carry; truncating the log folds it into the database before anything
    // reads it.
    let generation = {
        let conn = Connection::open(&source_db)?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        let backup_db = out.join(DB_FILE);
        let _ = std::fs::remove_file(&backup_db);
        conn.execute("VACUUM INTO ?1", [backup_db.to_string_lossy().as_ref()])?;
        let mut stmt = conn.prepare("SELECT id FROM migration ORDER BY id")?;
        let ids = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids
    };
    let backup_db = out.join(DB_FILE);
    let integrity = verify(&backup_db, DB_FILE)?;
    let (db_sha256, db_bytes) = digest_file(&backup_db)?;

    // Everything else the harness kept: auth stores, snapshot repos, plans,
    // the config it was given. Caches are left out — they are re-downloadable
    // by construction and would dominate the archive.
    let excluded = vec![
        materialize::OPENCODE_CACHE.to_string(),
        materialize::OPENCODE_DB.to_string(),
    ];
    let tar_path = out.join(TAR_FILE);
    let archived = archive(&staging, &tar_path, &excluded)?;
    let (tar_sha256, tar_file) = if archived > 0 {
        (Some(digest_file(&tar_path)?.0), Some(TAR_FILE.to_string()))
    } else {
        let _ = std::fs::remove_file(&tar_path);
        (None, None)
    };

    let identity = store.opencode_state_get(session_id)?;
    let workspace = checkpoint(backend, &workspace_id_of(&session)).await;

    let manifest = Manifest {
        manifest_version: 1,
        session_id: session_id.to_string(),
        harness_id: session.harness_id.clone(),
        build_version: identity
            .as_ref()
            .map(|i| i.build_version.clone())
            .or_else(|| session.harness_found.clone())
            .unwrap_or_else(|| session.harness_version.clone()),
        build_pinned: identity
            .as_ref()
            .map(|i| i.build_pinned.clone())
            .unwrap_or_else(|| session.harness_version.clone()),
        generation_digest: crate::store::generation_digest(&generation),
        generation,
        manifest_digest: identity.as_ref().and_then(|i| i.manifest_digest.clone()),
        state_volume: volume.clone(),
        state_path: materialize::HARNESS_TREE.to_string(),
        db_file: DB_FILE.to_string(),
        db_sha256,
        db_bytes,
        tar_file,
        tar_sha256,
        excluded,
        workspace,
        integrity,
        taken_ms,
        quiesced,
    };
    std::fs::write(
        out.join(MANIFEST_FILE),
        serde_json::to_vec_pretty(&manifest)?,
    )?;

    // The database just read is the authority on its own generation; whatever
    // the launch managed to record is superseded by it.
    if identity.is_some() {
        store.opencode_state_set_generation(session_id, &manifest.generation)?;
    }
    let _ = std::fs::remove_dir_all(&staging);
    Ok(BackupReport {
        path: out,
        manifest,
    })
}

/// Wait for the quiesced harness to actually let go. A stop returns when the
/// supervisor has taken it, not when the process is gone.
async fn wait_for_release(session_id: &str) -> Result<()> {
    let deadline = std::time::Instant::now() + QUIESCE_TIMEOUT;
    loop {
        if !materialize::state_claimed(session_id) {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(StateError::Claimed(session_id.to_string()));
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// Tar the state tree minus the excluded prefixes. Returns how many files went
/// in, so a tree with nothing but the database in it produces no archive at all
/// rather than an empty one the manifest would have to explain.
fn archive(root: &Path, out: &Path, excluded: &[String]) -> Result<usize> {
    let tree = root.join(materialize::HARNESS_TREE);
    if !tree.is_dir() {
        return Ok(0);
    }
    let skip: Vec<PathBuf> = excluded.iter().map(|e| root.join(e)).collect();
    let file = std::fs::File::create(out)?;
    let mut builder = tar::Builder::new(file);
    let mut count = 0;
    let mut stack = vec![tree];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)?.flatten() {
            let path = entry.path();
            if skip.iter().any(|s| path.starts_with(s)) {
                continue;
            }
            let meta = std::fs::symlink_metadata(&path)?;
            if meta.file_type().is_symlink() {
                // A link is not followed and not archived: a restore that
                // recreated one would be writing a path the node did not
                // choose into runtime-owned storage.
                continue;
            }
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            // The database's own sidecars belong to the copy, not the archive.
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("opencode.db-"))
            {
                continue;
            }
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            builder.append_path_with_name(&path, rel)?;
            count += 1;
        }
    }
    builder.finish()?;
    Ok(count)
}

// ---------------------------------------------------------------------------
// Upgrade on a clone
// ---------------------------------------------------------------------------

/// The target OpenCode binary on this host, if it is here at all.
///
/// A version is what is asked for, not a path: the point of the command is to
/// migrate onto a specific release, and a binary that reports something else
/// proves nothing about the release the migration was meant for.
pub fn host_binary(version: &str) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(named) = std::env::var_os("TRACON_OPENCODE_BINARY") {
        candidates.push(PathBuf::from(named));
    }
    candidates.push(
        Config::state_dir()
            .join("opencode")
            .join(version)
            .join("opencode"),
    );
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|dir| dir.join("opencode")));
    }
    candidates.into_iter().find(|candidate| {
        candidate.is_file()
            && std::process::Command::new(candidate)
                .arg("--version")
                .output()
                .ok()
                .is_some_and(|out| String::from_utf8_lossy(&out.stdout).trim() == version.trim())
    })
}

/// Migrate a session's state onto a newer OpenCode release, on a clone.
///
/// The original is not opened by the target binary at any point. If the clone
/// comes back corrupt, or the target binary is not on this host, the session's
/// state is exactly what it was.
pub async fn upgrade_state(
    store: &Store,
    backend: &dyn Backend,
    session_id: &str,
    to_version: &str,
) -> Result<UpgradeReport> {
    let session = store
        .get_session(session_id)?
        .ok_or_else(|| StateError::NotFound(session_id.to_string()))?;
    let identity = store.opencode_state_get(session_id)?.ok_or_else(|| {
        StateError::Rejected(format!(
            "session {session_id} has no recorded OpenCode state identity; \
             nothing is known about what would be migrated"
        ))
    })?;

    // The gate: a migration with no backup of the generation it starts from is
    // a migration with no way back.
    let has_backup = list_backups(session_id)
        .iter()
        .any(|(_, m)| m.generation_digest == identity.generation_digest);
    if !has_backup {
        return Err(StateError::NoBackup(format!(
            "no backup of session {session_id} at its current state generation \
             ({}); run `tracon session backup {session_id}` first",
            if identity.generation_digest.is_empty() {
                "unrecorded".to_string()
            } else {
                identity.generation_digest[..12].to_string()
            }
        )));
    }

    let Some(binary) = host_binary(to_version) else {
        return Err(StateError::NoBinary(format!(
            "no OpenCode {to_version} on this host: nothing was changed. \
             Put that build on PATH as `opencode`, name it in TRACON_OPENCODE_BINARY, \
             or install it under {}",
            Config::state_dir()
                .join("opencode")
                .join(to_version)
                .display()
        )));
    };

    if !SessionState::from_stored(&session.state).is_terminal() {
        return Err(StateError::Running {
            session: session_id.to_string(),
            state: session.state.clone(),
        });
    }
    let _fence = Fence::take(session_id)?;

    let volume = crate::workspace::scratch_volume_name(session_id);
    let work = work_dir(session_id).join("upgrade");
    let _ = std::fs::remove_dir_all(&work);
    let staging = work.join("export");
    {
        let lock = crate::workspace::volume_lock(&volume);
        let _guard = lock.lock().await;
        backend
            .export_volume(&volume, &staging)
            .await
            .map_err(|e| {
                StateError::Rejected(format!("the session's state could not be read: {e}"))
            })?;
    }
    let source_db = staging.join(materialize::OPENCODE_DB);
    if !source_db.is_file() {
        return Err(StateError::Rejected(format!(
            "session {session_id} has no OpenCode database to migrate"
        )));
    }
    let from_generation = read_generation(&source_db)?;

    // The clone. Made with `VACUUM INTO` rather than a file copy so the WAL
    // cannot be left behind, and so the clone is a database in its own right.
    let clone = work.join("clone").join("opencode.db");
    std::fs::create_dir_all(clone.parent().unwrap())?;
    {
        let conn = Connection::open(&source_db)?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        conn.execute("VACUUM INTO ?1", [clone.to_string_lossy().as_ref()])?;
    }

    apply_migrations_with(&binary, &work.join("home"), &clone).await?;

    // The target build left its migrations in the clone's write-ahead log.
    // Fold them in before anything reads the clone or copies it: a plain file
    // copy takes the database and not its log, which would silently drop the
    // very migrations this command exists to apply.
    {
        let conn = Connection::open(&clone)?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    }

    // Integrity before generation: a clone that did not survive its own
    // migration has no generation worth reading.
    verify(&clone, "the migrated clone").map_err(|e| {
        StateError::Rejected(format!(
            "{e}; the session's state was not touched and is still at {}",
            identity.build_version
        ))
    })?;
    let to_generation = read_generation(&clone)?;
    let known: BTreeSet<&String> = from_generation.iter().collect();
    let applied: Vec<String> = to_generation
        .iter()
        .filter(|id| !known.contains(id))
        .cloned()
        .collect();

    // Only now does the session's own state change: the verified clone
    // replaces the database in the staged tree, and the tree goes back into
    // runtime-owned storage as one import.
    std::fs::copy(&clone, &source_db)?;
    for stale in ["opencode.db-wal", "opencode.db-shm"] {
        let _ = std::fs::remove_file(source_db.with_file_name(stale));
    }
    {
        let lock = crate::workspace::volume_lock(&volume);
        let _guard = lock.lock().await;
        backend
            .import_volume(&volume, &staging)
            .await
            .map_err(|e| {
                StateError::Rejected(format!("the migrated state could not be written back: {e}"))
            })?;
    }

    store.opencode_state_put(&OpenCodeStateRow {
        session_id: session_id.to_string(),
        build_version: to_version.to_string(),
        build_pinned: identity.build_pinned.clone(),
        generation: to_generation.clone(),
        generation_digest: String::new(),
        manifest_digest: identity.manifest_digest.clone(),
        state_volume: volume,
        state_path: materialize::HARNESS_TREE.to_string(),
        recorded_ms: identity.recorded_ms,
        updated_ms: now_ms(),
    })?;

    Ok(UpgradeReport {
        session_id: session_id.to_string(),
        from_version: identity.build_version,
        to_version: to_version.to_string(),
        from_generation,
        to_generation,
        applied,
        clone_path: clone,
    })
}

/// Run an OpenCode binary against one database for long enough that it applies
/// its own migrations, and no longer.
///
/// It is a server because that is what a session runs and what the release
/// therefore migrates for; it is health-only because nothing is asked of it
/// beyond having opened the database. Egress is denied by environment rather
/// than by a boundary: this is a host-side throwaway, so every network-touching
/// path OpenCode has is switched off explicitly (`config-state.md` §8) and the
/// process is given no credential to reach a provider with.
pub async fn apply_migrations_with(binary: &Path, home: &Path, clone: &Path) -> Result<()> {
    std::fs::create_dir_all(home)?;
    let root = home.join(".opencode");
    let run = root.join("run");
    for dir in ["home", "config", "data", "cache", "state"] {
        std::fs::create_dir_all(run.join(dir))?;
    }
    // OpenCode mkdirs its XDG tree at import and resolves `OPENCODE_CONFIG` as
    // a file; a config that does not exist is a discovery path left open, so
    // an empty object is written rather than nothing.
    let config = root.join("opencode.json");
    std::fs::write(&config, "{}\n")?;
    let models = root.join("models.json");
    std::fs::write(&models, "{}\n")?;

    let port = pick_port()?;
    let password = uuid::Uuid::now_v7().to_string();
    let mut child = tokio::process::Command::new(binary)
        .args([
            "serve",
            "--hostname",
            "127.0.0.1",
            "--port",
            &port.to_string(),
        ])
        .current_dir(home)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", run.join("home"))
        .env("XDG_CONFIG_HOME", run.join("config"))
        .env("XDG_DATA_HOME", run.join("data"))
        .env("XDG_CACHE_HOME", run.join("cache"))
        .env("XDG_STATE_HOME", run.join("state"))
        .env("OPENCODE_DB", clone)
        .env("OPENCODE_CONFIG", &config)
        .env("OPENCODE_MODELS_PATH", &models)
        // Both halves of the credential. The server runs unsecured when the
        // password is unset, and states the username rather than defaulting it
        // so what is sent below is what was set.
        .env("OPENCODE_SERVER_USERNAME", SERVER_USERNAME)
        .env("OPENCODE_SERVER_PASSWORD", &password)
        // Every non-inference egress path upstream has, off.
        .env("OPENCODE_DISABLE_MODELS_FETCH", "true")
        .env("OPENCODE_DISABLE_AUTOUPDATE", "true")
        .env("OPENCODE_DISABLE_LSP_DOWNLOAD", "true")
        .env("OPENCODE_DISABLE_SHARE", "true")
        .env("OPENCODE_DISABLE_PROJECT_CONFIG", "true")
        .env("OPENCODE_DISABLE_EXTERNAL_SKILLS", "true")
        .env("OPENCODE_DISABLE_CLAUDE_CODE", "true")
        .env("OPENCODE_PURE", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;

    let health = format!("http://127.0.0.1:{port}/global/health");
    let client = reqwest::Client::new();
    let deadline = std::time::Instant::now() + UPGRADE_TIMEOUT;
    let mut healthy = false;
    while std::time::Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(StateError::Rejected(format!(
                "the OpenCode server exited before it finished migrating ({status}); \
                 the session's state was not touched"
            )));
        }
        // Bounded per request as well as overall: a server that accepts the
        // connection and then never answers would otherwise hold this loop
        // past its own deadline.
        if client
            .get(&health)
            .basic_auth(SERVER_USERNAME, Some(&password))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            healthy = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    // Stopped the way a session is: a signal, then the drop that kills what is
    // left. Migrations complete at import, long before health answers, so a
    // healthy server has already applied them.
    let _ = child.start_kill();
    let _ = child.wait().await;
    if !healthy {
        return Err(StateError::Rejected(
            "the OpenCode server never became healthy against the clone; \
             the session's state was not touched"
                .into(),
        ));
    }
    Ok(())
}

fn pick_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

// ---------------------------------------------------------------------------
// Restore
// ---------------------------------------------------------------------------

/// What a restore would do, before it does it. Separated so the gate can be
/// asked without the write, which is what the CLI prints and what the tests
/// assert on.
pub async fn restore(
    store: &Store,
    backend: &dyn Backend,
    cfg: &Config,
    session_id: &str,
    backup: &Path,
    confirm: bool,
) -> Result<RestoreReport> {
    let session = store
        .get_session(session_id)?
        .ok_or_else(|| StateError::NotFound(session_id.to_string()))?;
    let manifest = read_manifest(backup)?.ok_or_else(|| {
        StateError::Rejected(format!(
            "{} holds no {MANIFEST_FILE}; it is not a tracon backup",
            backup.display()
        ))
    })?;
    if manifest.session_id != session_id {
        return Err(StateError::Rejected(format!(
            "that backup belongs to session {}, not {session_id}",
            manifest.session_id
        )));
    }

    // Gate one: the build. A backup made by a newer OpenCode than this node
    // pins is refused outright — the older binary would open it, understand
    // none of it, and write to it anyway (`config-state.md` §9 row 7).
    let pinned = crate::adapter::pinned_version(cfg);
    if !at_least(&pinned, &manifest.build_version) {
        return Err(StateError::Rejected(format!(
            "that backup was made by OpenCode {} and this node pins {pinned}. \
             An older binary opens a newer state silently and can write to it, \
             so there is no downgrade path: restore it on a node pinned to {} or later",
            manifest.build_version, manifest.build_version
        )));
    }

    // Gate two: the generation. The pin agreeing is not the same as this
    // runtime knowing every migration the backup carries, and it is the
    // migrations that decide whether the schema is readable.
    let expected = store.opencode_state_expected(&pinned)?;
    match &expected {
        Some(expected) if expected.covers(&manifest.generation) => {}
        Some(expected) => {
            let missing: Vec<&String> = manifest
                .generation
                .iter()
                .filter(|id| !expected.generation.contains(id))
                .collect();
            return Err(StateError::Rejected(format!(
                "that backup's state generation is not one this node's OpenCode {pinned} \
                 produces: it carries {} migration(s) this runtime has never applied ({}). \
                 Restoring it would hand an older binary a newer schema, which it would \
                 read wrongly and still write to",
                missing.len(),
                missing
                    .iter()
                    .take(3)
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        None if manifest.generation.is_empty() => {}
        None => {
            return Err(StateError::Rejected(format!(
                "this node has no recorded state generation for OpenCode {pinned}, \
                 so there is nothing to check that backup's generation against. \
                 Start a session on this pin first"
            )));
        }
    }

    // Gate three: nobody may be writing it.
    if !SessionState::from_stored(&session.state).is_terminal() {
        return Err(StateError::Running {
            session: session_id.to_string(),
            state: session.state.clone(),
        });
    }
    if materialize::state_claimed(session_id) {
        return Err(StateError::Claimed(session_id.to_string()));
    }

    // Gate four: the workspace. The backup restores what the session knew, not
    // where it was working; a workspace that has moved since makes the two
    // disagree, and that is the operator's call rather than the node's.
    let now = checkpoint(backend, &workspace_id_of(&session)).await;
    let moved = manifest.workspace.moved_from(&now);
    if moved && !confirm {
        return Err(StateError::Rejected(format!(
            "the workspace has moved since this backup was taken ({} then, {} now). \
             The restore brings back the harness database, not the workspace, so the \
             session would resume believing in files that are no longer there. \
             Re-run with --confirm to accept that",
            manifest.workspace.commit.as_deref().unwrap_or("unreadable"),
            now.commit.as_deref().unwrap_or("unreadable"),
        )));
    }

    let _fence = Fence::take(session_id)?;

    let db = backup.join(&manifest.db_file);
    let (sha, _) = digest_file(&db)?;
    if sha != manifest.db_sha256 {
        return Err(StateError::Rejected(format!(
            "the backup's database does not match its manifest digest; it has been \
             altered or truncated since it was taken ({} on disk)",
            &sha[..12]
        )));
    }
    verify(&db, "the backup database")?;

    let volume = crate::workspace::scratch_volume_name(session_id);
    let staging = work_dir(session_id).join("restore");
    let _ = std::fs::remove_dir_all(&staging);
    {
        let lock = crate::workspace::volume_lock(&volume);
        let _guard = lock.lock().await;
        // The live tree is the base: a restore replaces the state the backup
        // covers and leaves the caches and anything newer in place rather than
        // emptying the volume.
        let _ = backend.export_volume(&volume, &staging).await;
    }
    let target = staging.join(materialize::OPENCODE_DB);
    std::fs::create_dir_all(target.parent().unwrap())?;
    std::fs::copy(&db, &target)?;
    for stale in ["opencode.db-wal", "opencode.db-shm"] {
        let _ = std::fs::remove_file(target.with_file_name(stale));
    }
    if let Some(tar_file) = &manifest.tar_file {
        let archive = std::fs::File::open(backup.join(tar_file))?;
        let mut tar = tar::Archive::new(archive);
        tar.set_overwrite(true);
        tar.unpack(&staging)?;
        // The archive never carries the database; the verified copy above is
        // the only thing that writes it.
        std::fs::copy(&db, &target)?;
    }
    {
        let lock = crate::workspace::volume_lock(&volume);
        let _guard = lock.lock().await;
        backend
            .import_volume(&volume, &staging)
            .await
            .map_err(|e| {
                StateError::Rejected(format!("the restored state could not be written: {e}"))
            })?;
    }
    let _ = std::fs::remove_dir_all(&staging);

    store.opencode_state_put(&OpenCodeStateRow {
        session_id: session_id.to_string(),
        build_version: manifest.build_version.clone(),
        build_pinned: pinned.clone(),
        generation: manifest.generation.clone(),
        generation_digest: String::new(),
        manifest_digest: manifest.manifest_digest.clone(),
        state_volume: volume,
        state_path: materialize::HARNESS_TREE.to_string(),
        recorded_ms: now_ms(),
        updated_ms: now_ms(),
    })?;

    Ok(RestoreReport {
        session_id: session_id.to_string(),
        backup: backup.to_path_buf(),
        build_version: manifest.build_version.clone(),
        generation_digest: manifest.generation_digest.clone(),
        preserves: preserves(&manifest),
        loses: loses(&manifest, &now),
        workspace_moved: moved,
    })
}

/// What the restore brings back, in the operator's terms.
pub fn preserves(manifest: &Manifest) -> Vec<String> {
    let mut said = vec![format!(
        "the harness database as of {} — sessions, messages, parts, permissions and saved \
         \"always\" grants (integrity {})",
        stamp(manifest.taken_ms),
        manifest.integrity
    )];
    if manifest.tar_file.is_some() {
        said.push(
            "the rest of the session's state tree: auth stores, snapshot repos, plans and the \
             configuration it was launched with"
                .into(),
        );
    }
    said
}

/// What it does not, said plainly rather than left to be discovered.
pub fn loses(manifest: &Manifest, now: &WorkspaceCheckpoint) -> Vec<String> {
    let mut said = Vec::new();
    said.push(match (&manifest.workspace.commit, &now.commit) {
        (Some(then), Some(now)) if then == now => format!(
            "nothing in the workspace is restored; it is still at {} and any uncommitted \
             change made since the checkpoint stays as it is",
            &then[..then.len().min(12)]
        ),
        (Some(then), Some(now)) => format!(
            "the workspace is not restored: it was at {} at the checkpoint and is at {} now, \
             and every edit in between remains",
            &then[..then.len().min(12)],
            &now[..now.len().min(12)]
        ),
        _ => "the workspace is not restored, and its state at the checkpoint could not be \
              read, so what the session will find there is unknown"
            .into(),
    });
    said.push(
        "no side effect the session had outside its own state: commits pushed, reviews opened, \
         tools called, notifications sent, or anything a permission it was granted did"
            .into(),
    );
    if !manifest.excluded.is_empty() {
        said.push(format!(
            "the caches deliberately left out of the backup ({}); the next launch re-establishes them",
            manifest.excluded.join(", ")
        ));
    }
    said
}

fn stamp(ms: i64) -> String {
    let secs = ms / 1000;
    format!("epoch {secs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_version_gate_compares_numerically_not_lexically() {
        assert!(at_least("1.18.30", "1.18.30"));
        assert!(
            at_least("1.18.30", "1.18.9"),
            "9 < 30, lexically the reverse"
        );
        assert!(at_least("1.19.0", "1.18.30"));
        assert!(!at_least("1.18.29", "1.18.30"));
        assert!(!at_least("1.18.30", "2.0.0"));
        // A build with no digits is only "at least" itself.
        assert!(at_least("dev", "dev"));
        assert!(!at_least("dev", "1.18.30"));
    }

    /// A checkpoint that could not be read is movement, not stillness: the
    /// safe reading of "unknown" is "changed".
    #[test]
    fn an_unreadable_workspace_counts_as_moved() {
        let clean = WorkspaceCheckpoint {
            commit: Some("abc".into()),
            dirty_digest: Some("d".into()),
            ..Default::default()
        };
        assert!(!clean.moved_from(&clean.clone()));
        let dirty = WorkspaceCheckpoint {
            dirty_digest: Some("e".into()),
            ..clean.clone()
        };
        assert!(clean.moved_from(&dirty), "an uncommitted edit is movement");
        let unknown = WorkspaceCheckpoint {
            unavailable: Some("no HEAD".into()),
            ..Default::default()
        };
        assert!(clean.moved_from(&unknown));
        assert!(unknown.moved_from(&clean));
        assert!(
            !unknown.moved_from(&unknown.clone()),
            "a session that never had a workspace still has nothing that moved"
        );
    }

    /// The losses are stated whether or not anything moved: a restore that
    /// said nothing about the workspace would read as restoring it.
    #[test]
    fn a_restore_always_says_what_it_does_not_bring_back() {
        let manifest = Manifest {
            manifest_version: 1,
            session_id: "s".into(),
            harness_id: "opencode".into(),
            build_version: "1.18.30".into(),
            build_pinned: "1.18.30".into(),
            generation: vec!["m1".into()],
            generation_digest: "d".into(),
            manifest_digest: None,
            state_volume: "v".into(),
            state_path: "harness".into(),
            db_file: DB_FILE.into(),
            db_sha256: "x".into(),
            db_bytes: 1,
            tar_file: None,
            tar_sha256: None,
            excluded: vec!["harness/run/cache".into()],
            workspace: WorkspaceCheckpoint {
                commit: Some("0123456789abcdef".into()),
                dirty_digest: Some("d".into()),
                ..Default::default()
            },
            integrity: "ok".into(),
            taken_ms: 1_700_000_000_000,
            quiesced: false,
        };
        let same = manifest.workspace.clone();
        let said = loses(&manifest, &same);
        assert!(said.iter().any(|s| s.contains("workspace")));
        assert!(said.iter().any(|s| s.contains("side effect")));
        assert!(said.iter().any(|s| s.contains("cache")));
        assert!(preserves(&manifest)[0].contains("integrity ok"));
    }
}
