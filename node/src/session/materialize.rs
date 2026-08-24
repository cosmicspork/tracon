//! Per-session scratch config. Nothing tracon needs is ever written into a
//! project repo, so harness configuration is materialized here and mounted in.

use std::path::{Path, PathBuf};

use crate::{config::Config, runner::Mount};

pub struct Scratch {
    pub dir: PathBuf,
    pub mounts: Vec<Mount>,
}

/// The node-owned harness state directory and the credential database inside
/// it. Shared by sessions and by the startup model probe: without these the
/// harness cannot open a session at all, so a probe that omits them reports no
/// models and looks like a harness fault.
pub fn state_mounts() -> std::io::Result<Vec<Mount>> {
    let state = Config::harness_state_dir();
    std::fs::create_dir_all(state.join("agent"))?;
    let mut mounts = vec![Mount {
        source: state.to_string_lossy().into_owned(),
        target: "/root/.omp".into(),
        read_only: false,
    }];
    // Model credentials stay in the harness's own store; the node does not
    // broker them. Only the database itself is carried in.
    let home_omp = dirs::home_dir().unwrap_or_default().join(".omp/agent");
    for f in ["agent.db", "agent.db-wal", "agent.db-shm"] {
        let src = home_omp.join(f);
        if src.exists() {
            mounts.push(Mount {
                source: src.to_string_lossy().into_owned(),
                target: format!("/root/.omp/agent/{f}"),
                read_only: false,
            });
        }
    }
    Ok(mounts)
}

/// Build the scratch directory for one session and the mounts that carry it
/// into the runner.
///
/// The harness state directory is node-owned and otherwise empty. Mounting the
/// operator's whole `~/.omp` would drag in its `AGENTS.md`, which is a symlink
/// to the workspace README, and a bind mount over a symlink does not mask it.
pub fn scratch_for(session_id: &str, worktree: &Path) -> std::io::Result<Scratch> {
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
        let s = scratch_for("test-materialize", Path::new("/tmp/wt")).unwrap();
        let targets: Vec<&str> = s.mounts.iter().map(|m| m.target.as_str()).collect();
        assert!(targets.contains(&"/work"));
        assert!(targets.contains(&"/root/.omp/agent/config.yml"));
        assert!(targets.contains(&"/root/.gitconfig"));
        // The harness state dir is the node's own, never the operator's ~/.omp.
        let omp = s.mounts.iter().find(|m| m.target == "/root/.omp").unwrap();
        assert!(omp.source.contains("harness-state"));
        assert!(!omp.source.ends_with("/.omp"));
        // And nothing mounts an AGENTS.md into the session.
        assert!(!targets.iter().any(|t| t.contains("AGENTS.md")));
        assert!(std::fs::read_to_string(s.dir.join("omp/config.yml"))
            .unwrap()
            .contains("backend: off"));
        remove("test-materialize");
    }
}
