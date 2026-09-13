//! Per-session harness files staged into runtime-owned storage.  The node
//! copies these bounded files into a named volume/PVC; no runner specification
//! receives a host path.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::{
    adapter::{HarnessAdapter, Layout},
    config::Config,
    gateway::model::Wiring,
    runner::Mount,
    workspace,
};

// ---------------------------------------------------------------------------
// The single-writer fence
// ---------------------------------------------------------------------------
//
// A harness keeps its state in a SQLite database, and OpenCode puts no lock on
// that database at all: two processes can open the same file, coordination is
// nothing but WAL plus a five-second busy timeout, and an older binary opening
// a newer database proceeds silently and can still write to it
// (`docs/reference/opencode-v1.18.30/config-state.md` §7.2, verdict row 6b —
// "upstream provides nothing", the fencing is tracon's to build).
//
// Per-session `HOME`, `XDG_*` and `OPENCODE_DB` keep two *different* sessions
// out of one database by construction. What they do not stop is the same
// session's state tree being opened twice — a relaunch racing a live harness,
// a second node process on the same state directory — and that is precisely
// the shape the reference calls out. So the node takes an advisory lock on the
// session's staged state directory before that tree is staged, holds it for
// the session's life, and refuses the launch when someone else holds it. The
// lock is `flock` on a file next to the tree rather than inside it: the tree is
// re-staged on every launch, and a lock a restage can delete is not a lock.

/// A held fence. Dropping it closes the descriptor, which is what releases the
/// kernel lock — including when the process dies without unwinding.
struct StateLease {
    _file: File,
}

/// How long a claim insists before it calls the fence held. Long enough to
/// outlast a `fork` that has not reached its `exec` yet (see `claim_state`),
/// short enough that a launch blocked by a live session says so promptly.
const CLAIM_ATTEMPTS: usize = 40;
const CLAIM_WAIT: std::time::Duration = std::time::Duration::from_millis(25);

fn leases() -> &'static Mutex<HashMap<String, StateLease>> {
    static LEASES: OnceLock<Mutex<HashMap<String, StateLease>>> = OnceLock::new();
    LEASES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Where the fence for one session's state lives: beside the session's staged
/// tree, not inside it.
pub fn state_lock_path(session_id: &str) -> PathBuf {
    Config::state_dir()
        .join("sessions")
        .join(format!("{session_id}.writer"))
}

/// Take the fence, or say who holds it. Called before a session's state tree
/// is staged, so a launch that would open a database another live session is
/// writing is refused rather than allowed to corrupt it.
pub fn claim_state(session_id: &str) -> std::io::Result<()> {
    let path = state_lock_path(session_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut held = leases().lock().unwrap();
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)?;
    // Non-blocking, because the answer the operator needs is "something else
    // is writing this" rather than a hang — but retried briefly, because a
    // lock held for an instant is not a second writer. A `flock` lives on the
    // open file description, and every `fork` in this process (the node forks
    // constantly: runners, git, the harness itself) duplicates that
    // description into a child until the child `exec`s. Releasing the fence
    // and re-taking it inside that window would otherwise be refused by a
    // descriptor nobody is using.
    let mut why = None;
    for attempt in 0..CLAIM_ATTEMPTS {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            why = None;
            break;
        }
        why = Some(std::io::Error::last_os_error());
        if attempt + 1 < CLAIM_ATTEMPTS {
            std::thread::sleep(CLAIM_WAIT);
        }
    }
    if let Some(why) = why {
        let holder = std::fs::read_to_string(&path).unwrap_or_default();
        let holder = holder.trim();
        return Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            format!(
                "the state for session {session_id} is already open for writing{} ({why}); \
                 a second writer would share a database nothing locks \
                 (config-state.md §7.2 row 6b)",
                if holder.is_empty() {
                    String::new()
                } else {
                    format!(" by {holder}")
                }
            ),
        ));
    }
    // Who holds it, for the refusal the next claimant reads.
    let mut file = file;
    file.set_len(0)?;
    writeln!(
        file,
        "instance {} pid {}",
        crate::process::instance_id(),
        std::process::id()
    )?;
    file.flush()?;
    held.insert(session_id.to_string(), StateLease { _file: file });
    Ok(())
}

/// Give the fence back. The session's state is nobody's until it is claimed
/// again.
pub fn release_state(session_id: &str) {
    leases().lock().unwrap().remove(session_id);
}

/// Whether this process holds the fence for a session.
pub fn holds_state(session_id: &str) -> bool {
    leases().lock().unwrap().contains_key(session_id)
}

/// Whether *anyone* holds the fence — this process or another. `holds_state`
/// answers only for this process's own leases, and a backup or a restore has
/// to know whether some other node process is writing the state it is about to
/// copy or overwrite. A `flock` lives on the open file description, so a second
/// descriptor on the same file is refused even from inside this process, which
/// is what makes the probe answer for both cases at once.
///
/// Probed rather than claimed: the answer is wanted without taking the fence,
/// and a claim taken to ask a question would have to be given back — which,
/// between the release and the caller's real claim, is a window the thing this
/// fence exists to prevent could slip through.
pub fn state_claimed(session_id: &str) -> bool {
    if holds_state(session_id) {
        return true;
    }
    let path = state_lock_path(session_id);
    let Ok(file) = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
    else {
        // No lock file that can be opened is no fence being held.
        return false;
    };
    // The same brief retry `claim_state` uses, for the same reason: a `fork`
    // that has not reached its `exec` still carries a duplicate of a live
    // descriptor, and one instant's refusal is not a second writer.
    for attempt in 0..CLAIM_ATTEMPTS {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
            return false;
        }
        if attempt + 1 < CLAIM_ATTEMPTS {
            std::thread::sleep(CLAIM_WAIT);
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Where a session's harness state sits inside its scratch volume
// ---------------------------------------------------------------------------
//
// `config_mounts` stages every adapter-declared directory under `harness/` in
// the session's own scratch volume and mounts it at the harness's state
// directory. Backup, upgrade and restore all work on the volume rather than on
// a container path, so they need the volume-relative spelling — and they must
// not re-derive it from an adapter they were not launched with.

/// The volume-relative root of the staged harness state tree: everything the
/// harness has under its own state directory, config files included.
pub const HARNESS_TREE: &str = "harness";

/// The volume-relative per-session writable tree for OpenCode. The adapter
/// declares `run` as its one scratch directory and puts `HOME` and all four
/// XDG directories inside it; `opencode_state_tree_matches_the_adapter` keeps
/// this from drifting away from that declaration.
pub const OPENCODE_RUN: &str = "harness/run";

/// The volume-relative OpenCode database, the path `OPENCODE_DB` names inside
/// the runner.
pub const OPENCODE_DB: &str = "harness/run/state/opencode.db";

/// The volume-relative cache tree. Excluded from a backup: it is a download
/// cache (ripgrep, npm packages, URL skills) that the image or the next launch
/// re-establishes, and copying it makes a backup large without making it more
/// restorable.
pub const OPENCODE_CACHE: &str = "harness/run/cache";

pub struct Scratch {
    /// Node-only staging path. It is copied into `volume` before a runner sees
    /// it and never appears in a mount specification.
    pub dir: PathBuf,
    pub volume: String,
    pub mounts: Vec<Mount>,
    pub orientation_path: String,
}

pub const PODMAN_HARNESS_HOME: &str = "/root";
pub const HARNESS_STATE_VOLUME: &str = "tracon-harness-state";

pub fn state_target(home: &str, layout: Layout) -> String {
    format!("{home}/{}", layout.dir)
}

/// Persistent harness state is a runtime-owned volume. It intentionally starts
/// empty: historical host state could contain provider credentials or arbitrary
/// configuration and is never imported into a harness.
pub fn state_mounts(home: &str, layout: Layout) -> std::io::Result<Vec<Mount>> {
    Ok(vec![Mount::volume(
        HARNESS_STATE_VOLUME,
        state_target(home, layout),
        false,
    )])
}

/// A pre-volume credential database is retired rather than copied forward.
pub fn retire_harness_credentials() -> Option<PathBuf> {
    let db = Config::harness_state_dir().join("agent/agent.db");
    if !db.exists() {
        return None;
    }
    let retired = db.with_extension("db.retired");
    if std::fs::rename(&db, &retired).is_err() {
        return None;
    }
    for stale in ["agent.db-wal", "agent.db-shm"] {
        let _ = std::fs::remove_file(db.with_file_name(stale));
    }
    Some(retired)
}

fn config_mounts(
    volume: &str,
    dir: &Path,
    home: &str,
    adapter: &dyn HarnessAdapter,
    wiring: &Wiring,
) -> std::io::Result<Vec<Mount>> {
    let root = state_target(home, adapter.layout());
    let mut mounts = Vec::new();
    // Directories the harness must be able to write, from this session's own
    // scratch volume rather than the state volume every session shares: a
    // harness whose home, caches and database are per-session keeps them
    // somewhere that dies with the session.
    for rel in adapter.scratch_dirs() {
        std::fs::create_dir_all(dir.join("harness").join(&rel))?;
        mounts.push(Mount::at(
            volume,
            format!("harness/{rel}"),
            format!("{root}/{rel}"),
            false,
        ));
    }
    for (rel, contents) in adapter.scratch_files(wiring) {
        let staged = dir.join("harness").join(&rel);
        if let Some(parent) = staged.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(staged, contents)?;
        mounts.push(Mount::at(
            volume,
            format!("harness/{rel}"),
            format!("{root}/{rel}"),
            true,
        ));
    }
    Ok(mounts)
}

/// Probe configuration staged in the same named-volume form as a session.
pub fn probe_scratch(
    home: &str,
    adapter: &dyn HarnessAdapter,
    wiring: &Wiring,
) -> std::io::Result<Scratch> {
    let volume = "tracon-probe-scratch".to_string();
    let dir = Config::state_dir().join("probe");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let mut mounts = state_mounts(home, adapter.layout())?;
    mounts.extend(config_mounts(&volume, &dir, home, adapter, wiring)?);
    Ok(Scratch {
        dir,
        volume,
        mounts,
        orientation_path: String::new(),
    })
}

/// Compatibility convenience for callers that only need the mount model.
pub fn probe_mounts(
    home: &str,
    adapter: &dyn HarnessAdapter,
    wiring: &Wiring,
) -> std::io::Result<Vec<Mount>> {
    Ok(probe_scratch(home, adapter, wiring)?.mounts)
}

/// Build configuration for a single session. `repo` is deliberately ignored:
/// workspaces are independently staged before launch and their Git directory is
/// never exposed by a same-path host mount.
pub fn scratch_for(
    session_id: &str,
    _worktree: &Path,
    _repo: &Path,
    home: &str,
    adapter: &dyn HarnessAdapter,
    wiring: &Wiring,
    orientation: &str,
) -> std::io::Result<Scratch> {
    // Before anything is staged: the tree about to be written holds this
    // session's harness database, and re-staging it under a live harness is
    // the corruption row 6b describes. A refusal here fails the launch.
    claim_state(session_id)?;
    let dir = Config::state_dir().join("sessions").join(session_id);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let volume = workspace::scratch_volume_name(session_id);

    std::fs::write(dir.join("orientation.md"), orientation)?;
    std::fs::write(
        dir.join("gitconfig"),
        "[user]\n\tname = tracon\n\temail = tracon@localhost\n[safe]\n\tdirectory = /work\n[advice]\n\tdetachedHead = false\n",
    )?;
    let orientation_path = format!("{}/orientation.md", state_target(home, adapter.layout()));

    let mut mounts = state_mounts(home, adapter.layout())?;
    mounts.extend(config_mounts(&volume, &dir, home, adapter, wiring)?);
    mounts.push(Mount::at(
        &volume,
        "orientation.md",
        orientation_path.clone(),
        true,
    ));
    mounts.push(Mount::at(
        &volume,
        "gitconfig",
        format!("{home}/.gitconfig"),
        true,
    ));

    Ok(Scratch {
        dir,
        volume,
        mounts,
        orientation_path,
    })
}

pub fn remove(session_id: &str) {
    let dir = Config::state_dir().join("sessions").join(session_id);
    let _ = std::fs::remove_dir_all(dir);
    // The session is over: its state is nobody's writer now, and the next
    // launch of this id may have it.
    release_state(session_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materialized_files_are_runtime_volume_mounts() {
        let scratch = scratch_for(
            "test-materialize",
            Path::new("/ignored"),
            Path::new("/ignored"),
            PODMAN_HARNESS_HOME,
            &crate::adapter::omp::OmpAdapter::new("18.0.4"),
            &Wiring::default(),
            "# Orientation",
        )
        .unwrap();
        assert!(scratch.mounts.iter().all(|mount| !mount.volume.is_empty()));
        assert!(scratch.mounts.iter().all(|mount| !mount.target.is_empty()));
        assert!(!scratch.mounts.iter().any(|mount| mount.target == "/work"));
    }

    /// Upstream locks nothing (`config-state.md` §7.2, row 6b), so this is the
    /// lock. A second claim on a session whose state is already open for
    /// writing is refused — and the refusal says who holds it — while a
    /// released one can be claimed again.
    #[test]
    fn one_session_state_has_one_writer() {
        let session = "test-fence-one-writer";
        release_state(session);
        claim_state(session).expect("the first writer takes it");
        assert!(holds_state(session));
        let refused = claim_state(session).expect_err("a second writer must be refused");
        assert_eq!(refused.kind(), std::io::ErrorKind::WouldBlock);
        assert!(
            refused.to_string().contains("pid") && refused.to_string().contains(session),
            "the refusal must name the session and its holder: {refused}"
        );
        // The lock is on a file beside the staged tree, not inside it, so
        // re-staging the tree cannot delete the thing that fences it.
        let lock = state_lock_path(session);
        assert!(lock.exists());
        assert!(!lock.starts_with(Config::state_dir().join("sessions").join(session)));

        release_state(session);
        assert!(!holds_state(session));
        claim_state(session).expect("a released fence can be taken again");
        release_state(session);
    }

    /// A probe answers for every process, not just this one, and answers
    /// without taking the fence it is asking about.
    #[test]
    fn the_fence_can_be_asked_about_without_being_taken() {
        let session = "test-fence-probe";
        release_state(session);
        assert!(!state_claimed(session));
        claim_state(session).expect("the probe left it takeable");
        assert!(state_claimed(session));
        release_state(session);
        assert!(!state_claimed(session));
        // And asking twice in a row does not leave it held by the asker.
        assert!(!state_claimed(session));
        claim_state(session).expect("still takeable after two probes");
        release_state(session);
    }

    /// The volume-relative paths backup and restore work on are the ones
    /// staging actually produces for OpenCode. The adapter declares the tree;
    /// this asserts the spelling here did not drift away from that.
    #[test]
    fn the_opencode_state_tree_matches_the_adapter() {
        let adapter = crate::adapter::opencode::OpenCodeAdapter::new("1.18.30");
        assert_eq!(adapter.scratch_dirs(), vec!["run".to_string()]);
        assert_eq!(OPENCODE_RUN, format!("{HARNESS_TREE}/run"));
        assert_eq!(OPENCODE_DB, format!("{OPENCODE_RUN}/state/opencode.db"));
        assert_eq!(OPENCODE_CACHE, format!("{OPENCODE_RUN}/cache"));

        let session = "test-state-tree";
        release_state(session);
        let scratch = scratch_for(
            session,
            Path::new("/ignored"),
            Path::new("/ignored"),
            PODMAN_HARNESS_HOME,
            &adapter,
            &Wiring::default(),
            "# Orientation",
        )
        .unwrap();
        assert!(
            scratch
                .mounts
                .iter()
                .any(|m| m.sub_path == OPENCODE_RUN && m.volume == scratch.volume),
            "the run tree is mounted from the session's own scratch volume"
        );
        assert!(scratch.dir.join(OPENCODE_RUN).is_dir());
        remove(session);
    }

    /// Staging a session's files takes the fence, and removing them gives it
    /// back: the launch path and the teardown path are where it belongs.
    #[test]
    fn staging_takes_the_fence_and_removal_returns_it() {
        let session = "test-fence-staging";
        release_state(session);
        let staged = || {
            scratch_for(
                session,
                Path::new("/ignored"),
                Path::new("/ignored"),
                PODMAN_HARNESS_HOME,
                &crate::adapter::omp::OmpAdapter::new("18.0.4"),
                &Wiring::default(),
                "# Orientation",
            )
        };
        staged().expect("the first staging takes the fence");
        assert!(holds_state(session));
        let refused = match staged() {
            Err(e) => e,
            Ok(_) => panic!("a second staging under a live one must be refused"),
        };
        assert_eq!(refused.kind(), std::io::ErrorKind::WouldBlock);
        remove(session);
        assert!(!holds_state(session));
        staged().expect("the next launch of this id may have it");
        remove(session);
    }
}
