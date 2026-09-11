//! Per-session harness files staged into runtime-owned storage.  The node
//! copies these bounded files into a named volume/PVC; no runner specification
//! receives a host path.

use std::path::{Path, PathBuf};

use crate::{
    adapter::{HarnessAdapter, Layout},
    config::Config,
    gateway::model::Wiring,
    runner::Mount,
    workspace,
};

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
}
