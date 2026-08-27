//! Per-session scratch config. Nothing tracon needs is ever written into a
//! project repo, so harness configuration is materialized here and mounted in.

use std::path::{Path, PathBuf};

use crate::{config::Config, runner::Mount};

pub struct Scratch {
    pub dir: PathBuf,
    pub mounts: Vec<Mount>,
}

/// Where the harness keeps its state inside the runner. The node-owned volume
/// is mounted here; `OMP_STATE_DIR` is set to the same path so a harness that
/// honours it agrees with the mount.
pub const HARNESS_STATE_TARGET: &str = "/root/.omp";

/// The node-owned harness state directory, and nothing else. Model credentials
/// live in it (`agent/agent.db`), put there once by `tracon harness
/// import-credentials` or a login through `tracon harness shell`; the node
/// never reaches into the operator's own `~/.omp`.
pub fn state_mounts() -> std::io::Result<Vec<Mount>> {
    let state = Config::harness_state_dir();
    std::fs::create_dir_all(state.join("agent"))?;
    Ok(vec![Mount {
        source: state.to_string_lossy().into_owned(),
        target: HARNESS_STATE_TARGET.into(),
        read_only: false,
    }])
}

/// The credential database the harness reads.
pub fn credentials_path() -> PathBuf {
    Config::harness_state_dir().join("agent/agent.db")
}

/// Whether the volume holds a credential store at all.
pub fn has_credentials() -> bool {
    credentials_path().exists()
}

/// Copy a credential database into the volume through SQLite's online backup,
/// so a live WAL is folded into one consistent file. Refuses to overwrite
/// unless asked.
pub fn import_credentials(from: &Path, force: bool) -> Result<PathBuf, String> {
    let dest = credentials_path();
    if dest.exists() && !force {
        return Err(format!(
            "{} already exists; pass --force to replace it",
            dest.display()
        ));
    }
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let src =
        rusqlite::Connection::open_with_flags(from, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("open {}: {e}", from.display()))?;
    let tmp = dest.with_extension("db.tmp");
    let _ = std::fs::remove_file(&tmp);
    {
        let mut dst = rusqlite::Connection::open(&tmp).map_err(|e| e.to_string())?;
        let backup = rusqlite::backup::Backup::new(&src, &mut dst).map_err(|e| e.to_string())?;
        backup
            .run_to_completion(64, std::time::Duration::from_millis(5), None)
            .map_err(|e| e.to_string())?;
    }
    for stale in ["agent.db-wal", "agent.db-shm"] {
        let _ = std::fs::remove_file(dest.with_file_name(stale));
    }
    std::fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o600));
    }
    Ok(dest)
}

/// Build the scratch directory for one session and the mounts that carry it
/// into the runner.
///
/// The harness state directory is node-owned and otherwise empty. Mounting the
/// operator's whole `~/.omp` would drag in its `AGENTS.md`, which is a symlink
/// to the workspace README, and a bind mount over a symlink does not mask it.
pub fn scratch_for(session_id: &str, worktree: &Path, repo: &Path) -> std::io::Result<Scratch> {
    let dir = Config::state_dir().join("sessions").join(session_id);
    std::fs::create_dir_all(dir.join("omp"))?;

    // Memory is a Phase 4 concern and the node owns it; the harness's own
    // memory backend would write into state we do not model.
    std::fs::write(dir.join("omp/config.yml"), "memory:\n  backend: off\n")?;

    // `safe.directory` because the worktree is owned by the host user but git
    // runs as root inside the runner.
    std::fs::write(
        dir.join("gitconfig"),
        "[user]\n\tname = tracon\n\temail = tracon@localhost\n\
         [safe]\n\tdirectory = /work\n[advice]\n\tdetachedHead = false\n",
    )?;

    let mut mounts = state_mounts()?;
    mounts.extend([
        Mount {
            source: dir.join("omp/config.yml").to_string_lossy().into_owned(),
            target: "/root/.omp/agent/config.yml".into(),
            read_only: true,
        },
        Mount {
            source: dir.join("gitconfig").to_string_lossy().into_owned(),
            target: "/root/.gitconfig".into(),
            read_only: true,
        },
        Mount {
            source: worktree.to_string_lossy().into_owned(),
            target: "/work".into(),
            read_only: false,
        },
    ]);

    // A linked worktree's `.git` is a file pointing at
    // `<repo>/.git/worktrees/<name>`, an absolute host path. Without the repo's
    // git directory mounted at that same path, git inside the runner reports
    // "not a git repository" and the harness can edit files but never commit —
    // which also means it can never submit a review.
    //
    // Committing needs write access to objects, refs, logs, and the worktree's
    // own state — but NOT to `config`, `hooks`, or `info`. Those three are the
    // code-execution surface: a writable `.git/config` lets the harness set
    // `core.fsmonitor`, `core.hooksPath`, `credential.helper`, or an external
    // diff/textconv command that the node then runs host-side (during capture,
    // staleness, or publish) as the node's user — outside the boundary. So the
    // git directory is mounted read-write for commits, and those three paths are
    // layered back read-only on top so the harness cannot reach them. This is
    // the "nothing the harness writes can execute on the node" half of the gate;
    // node-side git is also invoked with hooks and drivers disabled as a second
    // line (see `review::git` and `worktree::git`).
    let git_dir = repo.join(".git");
    if git_dir.is_dir() {
        mounts.push(Mount {
            source: git_dir.to_string_lossy().into_owned(),
            target: git_dir.to_string_lossy().into_owned(),
            read_only: false,
        });
        // Read-only overlays on the execution surface. Each exists in a normal
        // repo (git creates them at init); guard anyway so a bare or unusual
        // layout does not fail the mount.
        for leaf in ["config", "hooks", "info"] {
            let p = git_dir.join(leaf);
            if p.exists() {
                mounts.push(Mount {
                    source: p.to_string_lossy().into_owned(),
                    target: p.to_string_lossy().into_owned(),
                    read_only: true,
                });
            }
        }
    }

    Ok(Scratch { dir, mounts })
}

pub fn remove(session_id: &str) {
    let dir = Config::state_dir().join("sessions").join(session_id);
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_carries_config_and_worktree_but_not_the_operator_state_dir() {
        let s = scratch_for(
            "test-materialize",
            Path::new("/tmp/wt"),
            Path::new("/tmp/repo"),
        )
        .unwrap();
        let targets: Vec<&str> = s.mounts.iter().map(|m| m.target.as_str()).collect();
        assert!(targets.contains(&"/work"));
        assert!(targets.contains(&"/root/.omp/agent/config.yml"));
        assert!(targets.contains(&"/root/.gitconfig"));
        // The harness state dir is the node's own, never the operator's ~/.omp,
        // and nothing from the operator's home is mounted alongside it.
        let omp = s.mounts.iter().find(|m| m.target == "/root/.omp").unwrap();
        assert!(omp.source.contains("harness-state"));
        assert!(!omp.source.ends_with("/.omp"));
        let home = dirs::home_dir().unwrap_or_default().join(".omp");
        assert!(s
            .mounts
            .iter()
            .all(|m| !m.source.starts_with(&home.to_string_lossy().into_owned())));
        // And nothing mounts an AGENTS.md into the session.
        assert!(!targets.iter().any(|t| t.contains("AGENTS.md")));
        assert!(std::fs::read_to_string(s.dir.join("omp/config.yml"))
            .unwrap()
            .contains("backend: off"));
        remove("test-materialize");
    }

    #[test]
    fn a_worktrees_git_directory_is_reachable_from_the_runner() {
        // Without this the harness cannot commit, and the review contract
        // depends on commits.
        let repo = std::env::temp_dir().join("tracon-materialize-repo");
        std::fs::create_dir_all(repo.join(".git/hooks")).unwrap();
        std::fs::create_dir_all(repo.join(".git/info")).unwrap();
        std::fs::write(repo.join(".git/config"), "[core]\n").unwrap();
        let s = scratch_for("test-gitdir", Path::new("/tmp/wt"), &repo).unwrap();
        let git_target = repo.join(".git").to_string_lossy().into_owned();
        let mount = s
            .mounts
            .iter()
            .find(|m| m.target == git_target)
            .expect("the repo's git directory should be mounted");
        // At the same absolute path, because the worktree's pointer is absolute.
        assert_eq!(mount.source, mount.target);
        assert!(!mount.read_only, "committing writes objects and refs");
        // config, hooks, and info are layered back read-only: they are the
        // node-side code-execution surface and the harness must not write them.
        for leaf in ["config", "hooks", "info"] {
            let target = repo.join(".git").join(leaf).to_string_lossy().into_owned();
            let ro = s
                .mounts
                .iter()
                .find(|m| m.target == target)
                .unwrap_or_else(|| panic!(".git/{leaf} should be mounted"));
            assert!(ro.read_only, ".git/{leaf} must be read-only to the harness");
        }
        remove("test-gitdir");
        let _ = std::fs::remove_dir_all(&repo);
    }
}
