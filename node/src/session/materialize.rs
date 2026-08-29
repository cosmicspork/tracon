//! Per-session scratch config. Nothing tracon needs is ever written into a
//! project repo, so harness configuration is materialized here and mounted in.

use std::path::{Path, PathBuf};

use crate::{
    adapter::{HarnessAdapter, Layout},
    config::Config,
    gateway::model::Wiring,
    runner::Mount,
};

pub struct Scratch {
    pub dir: PathBuf,
    pub mounts: Vec<Mount>,
    /// Where the harness finds the orientation text, inside the runner.
    pub orientation_path: String,
}

/// Where the harness keeps its state inside the runner. The node-owned volume
/// is mounted here; the harness's own state-dir variable is set to the same
/// path so a harness that honours it agrees with the mount.
pub const PODMAN_HARNESS_HOME: &str = "/root";

/// Where the harness's state directory is mounted, under its home. The name
/// is the harness's, not ours: omp looks for `.omp`, and a second adapter
/// looks for its own.
pub fn state_target(home: &str, layout: Layout) -> String {
    format!("{home}/{}", layout.dir)
}

/// The node-owned harness state directory, and nothing else: the harness's
/// own caches, sessions, and model catalogue. It holds no credential — the
/// model gateway injects those — and the node never reaches into the
/// operator's own `~/.omp`.
pub fn state_mounts(home: &str, layout: Layout) -> std::io::Result<Vec<Mount>> {
    let state = Config::harness_state_dir();
    std::fs::create_dir_all(state.join("agent"))?;
    Ok(vec![Mount {
        source: state.to_string_lossy().into_owned(),
        target: state_target(home, layout),
        read_only: false,
    }])
}

/// A credential store left on the volume by Phases 1–3 would let the harness
/// talk to a provider directly, around the gateway and its bindings. Set it
/// aside on startup rather than mount it; nothing deletes it.
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

/// The harness's own configuration — whatever it needs to find in its state
/// directory, which for omp is the provider override that points it at the
/// gateway and a config turning its memory backend off. Written under `dir`
/// mirroring the layout inside the runner, and mounted read-only: the harness
/// reads its configuration, it does not get to rewrite it.
fn config_mounts(
    dir: &Path,
    home: &str,
    adapter: &dyn HarnessAdapter,
    wiring: &Wiring,
) -> std::io::Result<Vec<Mount>> {
    let root = state_target(home, adapter.layout());
    let mut mounts = Vec::new();
    for (rel, contents) in adapter.scratch_files(wiring) {
        let host = dir.join(&rel);
        if let Some(parent) = host.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&host, contents)?;
        mounts.push(Mount {
            source: host.to_string_lossy().into_owned(),
            target: format!("{root}/{rel}"),
            read_only: true,
        });
    }
    Ok(mounts)
}

/// Mounts for the node's own model probe: the state volume plus the gateway
/// wiring, nothing of any session.
pub fn probe_mounts(
    home: &str,
    adapter: &dyn HarnessAdapter,
    wiring: &Wiring,
) -> std::io::Result<Vec<Mount>> {
    let mut mounts = state_mounts(home, adapter.layout())?;
    mounts.extend(config_mounts(
        &Config::state_dir().join("probe"),
        home,
        adapter,
        wiring,
    )?);
    Ok(mounts)
}

/// Build the scratch directory for one session and the mounts that carry it
/// into the runner.
///
/// The harness state directory is node-owned and otherwise empty. Mounting the
/// operator's whole `~/.omp` would drag in its `AGENTS.md`, which is a symlink
/// to the workspace README, and a bind mount over a symlink does not mask it.
pub fn scratch_for(
    session_id: &str,
    worktree: &Path,
    repo: &Path,
    home: &str,
    adapter: &dyn HarnessAdapter,
    wiring: &Wiring,
    orientation: &str,
) -> std::io::Result<Scratch> {
    let dir = Config::state_dir().join("sessions").join(session_id);
    std::fs::create_dir_all(&dir)?;

    // The orientation is a system-prompt file, mounted read-only under the
    // harness's state directory — never into the worktree.
    std::fs::write(dir.join("orientation.md"), orientation)?;
    let orientation_path = format!("{}/orientation.md", state_target(home, adapter.layout()));

    // `safe.directory` because the worktree is owned by the host user but git
    // runs as root inside the runner.
    std::fs::write(
        dir.join("gitconfig"),
        "[user]\n\tname = tracon\n\temail = tracon@localhost\n\
         [safe]\n\tdirectory = /work\n[advice]\n\tdetachedHead = false\n",
    )?;

    let mut mounts = state_mounts(home, adapter.layout())?;
    mounts.extend(config_mounts(&dir.join("harness"), home, adapter, wiring)?);
    mounts.push(Mount {
        source: dir.join("orientation.md").to_string_lossy().into_owned(),
        target: orientation_path.clone(),
        read_only: true,
    });
    mounts.extend([
        Mount {
            source: dir.join("gitconfig").to_string_lossy().into_owned(),
            target: format!("{home}/.gitconfig"),
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

    Ok(Scratch {
        dir,
        mounts,
        orientation_path,
    })
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
            PODMAN_HARNESS_HOME,
            &crate::adapter::omp::OmpAdapter::new("18.0.4"),
            &Wiring::default(),
            "# Orientation",
        )
        .unwrap();
        let targets: Vec<&str> = s.mounts.iter().map(|m| m.target.as_str()).collect();
        assert!(targets.contains(&"/work"));
        assert!(targets.contains(&"/root/.omp/orientation.md"));
        assert_eq!(s.orientation_path, "/root/.omp/orientation.md");
        assert!(targets.contains(&"/root/.omp/agent/config.yml"));
        assert!(targets.contains(&"/root/.omp/agent/models.json"));
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
        assert!(
            std::fs::read_to_string(s.dir.join("harness/agent/config.yml"))
                .unwrap()
                .contains("backend: off")
        );
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
        let s = scratch_for(
            "test-gitdir",
            Path::new("/tmp/wt"),
            &repo,
            PODMAN_HARNESS_HOME,
            &crate::adapter::omp::OmpAdapter::new("18.0.4"),
            &Wiring::default(),
            "",
        )
        .unwrap();
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
