//! Reclaiming runtime storage whose owner is over.
//!
//! The node creates runtime volumes on demand and, apart from a check run's,
//! never removes them itself: a session's workspace outlives it for resume
//! and export, and its harness state for backup and restore. Once nothing can
//! use one, it is only disk. A sweep names every `tracon-` volume and the
//! host directories that shadow them, says for each whether its owner is over
//! and why, and removes only when asked to.
//!
//! Ownership is read from the name. `tracon-scratch-<session>` belongs to a
//! session; `tracon-workspace-<id>` to every open session using that
//! workspace; `tracon-cache-w-<id>` to that same workspace, whose sessions
//! install into it; `tracon-check-<run>` to a check run, as `tracon-prep-…` and
//! `tracon-warm-…` are to the run that made them; `tracon-cache-<hash>` to
//! nobody (it is rebuilt on demand, so it goes only when asked for). The
//! node-wide volumes are never candidates, and neither is a name this node
//! does not recognize.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::boundary::Backend;
use crate::config::Config;
use crate::store::Store;

/// A workspace nothing references yet may be about to be: a browser import
/// is referenced only once a session starts on it.
pub const WORKSPACE_GRACE_MS: i64 = 24 * 60 * 60 * 1000;

const NODE_WIDE: [&str; 2] = [
    crate::session::materialize::HARNESS_STATE_VOLUME,
    "tracon-probe-scratch",
];

/// What a sweep knows about the owners of runtime storage.
#[derive(Debug, Default)]
pub struct Owners {
    /// Sessions on this node that are not archived.
    pub open_sessions: HashSet<String>,
    /// Every session this node has run.
    pub known_sessions: HashSet<String>,
    /// Workspace ids, as their volume names carry them, that an open session
    /// or a retryable import still uses.
    pub used_workspaces: HashSet<String>,
    pub running_checks: HashSet<String>,
    pub now_ms: i64,
    /// Dependency caches are candidates too.
    pub caches: bool,
}

impl Owners {
    pub fn read(
        store: &Store,
        node_id: &str,
        caches: bool,
    ) -> Result<Self, crate::store::StoreError> {
        let mut owners = Owners {
            now_ms: crate::store::now_ms(),
            caches,
            ..Default::default()
        };
        for session in store.list_sessions(None)? {
            if session.node_id != node_id {
                continue;
            }
            owners.known_sessions.insert(session.id.clone());
            if session.archived_ms.is_some() {
                continue;
            }
            owners.open_sessions.insert(session.id.clone());
            let workspace = session
                .repo_path
                .strip_prefix("workspace://")
                .unwrap_or(&session.id);
            owners.used_workspaces.insert(workspace_key(workspace));
        }
        for workspace in store.retryable_import_workspaces()? {
            owners.used_workspaces.insert(workspace_key(&workspace));
        }
        owners.running_checks = store.running_check_run_ids()?;
        Ok(owners)
    }
}

/// A workspace id as its volume name and host directories carry it.
fn workspace_key(id: &str) -> String {
    crate::workspace::volume_name(id)
        .trim_start_matches("tracon-workspace-")
        .to_string()
}

/// One piece of runtime storage and what the sweep makes of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Item {
    /// `volume` or `directory`.
    pub kind: &'static str,
    /// The volume name, or the directory relative to the node's state dir.
    pub name: String,
    pub remove: bool,
    pub reason: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub removed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn verdict(remove: bool, reason: impl Into<String>) -> (bool, String) {
    (remove, reason.into())
}

fn workspace_verdict(key: &str, created_ms: Option<i64>, owners: &Owners) -> (bool, String) {
    if owners.used_workspaces.contains(key) {
        return verdict(false, "an open session uses this workspace");
    }
    if created_ms.is_some_and(|created| owners.now_ms - created < WORKSPACE_GRACE_MS) {
        return verdict(
            false,
            "created in the last day; a session may be about to use it",
        );
    }
    verdict(true, "no open session uses this workspace")
}

fn session_verdict(id: &str, owners: &Owners, what: &str) -> (bool, String) {
    if owners.open_sessions.contains(id) {
        verdict(false, format!("session {id} is not archived"))
    } else if owners.known_sessions.contains(id) {
        verdict(true, format!("session {id} is archived; {what}"))
    } else {
        verdict(true, format!("no session {id} on this node"))
    }
}

/// Whether a runtime volume's owner is over.
pub fn classify_volume(name: &str, created_ms: Option<i64>, owners: &Owners) -> (bool, String) {
    if NODE_WIDE.contains(&name) {
        return verdict(false, "node-wide; every session uses it");
    }
    if let Some(session) = name.strip_prefix("tracon-scratch-") {
        return session_verdict(
            session,
            owners,
            "its harness state can no longer be backed up, upgraded or restored",
        );
    }
    if let Some(key) = name.strip_prefix("tracon-workspace-") {
        return workspace_verdict(key, created_ms, owners);
    }
    if let Some(run) = name.strip_prefix("tracon-check-") {
        return if owners.running_checks.contains(run) {
            verdict(false, "the check is running")
        } else {
            verdict(true, "the check run has finished")
        };
    }
    // What one check run or one cache warm-up made for itself, and removes
    // itself when it ends. One still here a day later was left by a run that
    // did not finish.
    if ["tracon-prep-", "tracon-warm-"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
    {
        return if created_ms.is_some_and(|created| owners.now_ms - created < WORKSPACE_GRACE_MS) {
            verdict(
                false,
                "made by a run in the last day; it may still be using it",
            )
        } else {
            verdict(true, "left by a run that did not finish")
        };
    }
    // A session's own cache lives and dies with its workspace; the
    // repositories' shared ones below go only when caches are asked for.
    if let Some(key) = name.strip_prefix("tracon-cache-w-") {
        return workspace_verdict(key, created_ms, owners);
    }
    if name.starts_with("tracon-cache-") {
        return if owners.caches {
            verdict(true, "dependency cache; rebuilt when next needed")
        } else {
            verdict(
                false,
                "dependency cache; included only when caches are asked for",
            )
        };
    }
    if name.starts_with("tracon-provider-") {
        return verdict(
            true,
            "a sign-in container's volume; providers sign in natively now",
        );
    }
    verdict(false, "not a volume this node recognizes")
}

/// Whether a host directory under the state dir is still needed. `area` is
/// the directory it sits in, `entry` its own name.
pub fn classify_directory(
    area: &str,
    entry: &str,
    created_ms: Option<i64>,
    owners: &Owners,
) -> Option<(bool, String)> {
    match area {
        "sessions" => Some(session_verdict(
            entry,
            owners,
            "its staged configuration is unused",
        )),
        "workspaces" | "workspace-staging" => Some(workspace_verdict(entry, created_ms, owners)),
        _ => None,
    }
}

/// Top-level directories no current code reads.
const RETIRED_DIRECTORIES: [(&str, &str); 2] = [
    (
        "providers",
        "sign-in container state; providers sign in natively now",
    ),
    (
        "harness-state",
        "harness state from before it moved into the runtime volume",
    ),
];

fn created_ms(path: &Path) -> Option<i64> {
    std::fs::metadata(path)
        .and_then(|m| m.created().or_else(|_| m.modified()))
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
}

fn directory_items(state_dir: &Path, owners: &Owners) -> Vec<(Item, PathBuf)> {
    let mut items = Vec::new();
    for (name, reason) in RETIRED_DIRECTORIES {
        let path = state_dir.join(name);
        if path.is_dir() {
            items.push((item("directory", name.into(), true, reason.into()), path));
        }
    }
    for area in ["sessions", "workspaces", "workspace-staging"] {
        let Ok(entries) = std::fs::read_dir(state_dir.join(area)) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let entry = entry.file_name().to_string_lossy().into_owned();
            if let Some((remove, reason)) =
                classify_directory(area, &entry, created_ms(&path), owners)
            {
                items.push((
                    item("directory", format!("{area}/{entry}"), remove, reason),
                    path,
                ));
            }
        }
    }
    items
}

fn item(kind: &'static str, name: String, remove: bool, reason: String) -> Item {
    Item {
        kind,
        name,
        remove,
        reason,
        removed: false,
        error: None,
    }
}

/// Name every piece of runtime storage and, with `apply`, remove the ones
/// whose owner is over. A volume a harness still mounts is refused by the
/// backend and reported, not forced.
pub async fn sweep(
    backend: &dyn Backend,
    store: &Store,
    node_id: &str,
    caches: bool,
    apply: bool,
) -> Result<Vec<Item>, String> {
    let owners = Owners::read(store, node_id, caches).map_err(|e| e.to_string())?;
    let volumes = backend.list_volumes().await.map_err(|e| e.to_string())?;
    let mut items = Vec::new();
    for volume in volumes {
        let (remove, reason) = classify_volume(&volume.name, volume.created_ms, &owners);
        let mut item = item("volume", volume.name, remove, reason);
        if apply && remove {
            let lock = crate::workspace::volume_lock(&item.name);
            let _guard = lock.lock().await;
            match backend.remove_volume(&item.name).await {
                Ok(()) => item.removed = true,
                Err(error) => item.error = Some(error.to_string()),
            }
        }
        items.push(item);
    }
    for (mut item, path) in directory_items(&Config::state_dir(), &owners) {
        if apply && item.remove {
            match std::fs::remove_dir_all(&path) {
                Ok(()) => item.removed = true,
                Err(error) => item.error = Some(error.to_string()),
            }
        }
        items.push(item);
    }
    items.sort_by(|a, b| b.remove.cmp(&a.remove).then_with(|| a.name.cmp(&b.name)));
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owners() -> Owners {
        let mut owners = Owners {
            now_ms: 10 * WORKSPACE_GRACE_MS,
            ..Default::default()
        };
        owners
            .known_sessions
            .extend(["open".into(), "archived".into()]);
        owners.open_sessions.insert("open".into());
        owners.used_workspaces.insert("open".into());
        owners.used_workspaces.insert("shared".into());
        owners.running_checks.insert("live".into());
        owners
    }

    #[test]
    fn a_session_s_volumes_go_once_it_is_archived_or_unknown() {
        let o = owners();
        assert!(!classify_volume("tracon-scratch-open", None, &o).0);
        assert!(classify_volume("tracon-scratch-archived", None, &o).0);
        let (remove, reason) = classify_volume("tracon-scratch-gone", None, &o);
        assert!(remove);
        assert!(reason.contains("no session gone"));
    }

    #[test]
    fn a_workspace_stays_while_an_open_session_uses_it_or_it_is_new() {
        let o = owners();
        let old = Some(0);
        assert!(!classify_volume("tracon-workspace-open", old, &o).0);
        assert!(!classify_volume("tracon-workspace-shared", old, &o).0);
        assert!(classify_volume("tracon-workspace-archived", old, &o).0);
        assert!(!classify_volume("tracon-workspace-imported", Some(o.now_ms - 1000), &o).0);
        assert!(classify_volume("tracon-workspace-imported", None, &o).0);
        // A run's own volumes are its to remove; one a day old was left.
        for left in [
            "tracon-prep-0199",
            "tracon-prep-cache-0199",
            "tracon-warm-0199",
        ] {
            assert!(classify_volume(left, old, &o).0, "{left}");
            assert!(
                !classify_volume(left, Some(o.now_ms - 1000), &o).0,
                "{left}"
            );
        }
        // What its sessions installed goes when it does, caches asked for or
        // not, and never before.
        assert!(!classify_volume("tracon-cache-w-open", old, &o).0);
        assert!(classify_volume("tracon-cache-w-archived", old, &o).0);
    }

    #[test]
    fn node_wide_caches_and_unknown_volumes_stay_unless_asked() {
        let mut o = owners();
        for name in [
            "tracon-harness-state",
            "tracon-probe-scratch",
            "tracon-something-new",
        ] {
            assert!(!classify_volume(name, None, &o).0, "{name}");
        }
        assert!(!classify_volume("tracon-cache-abc", None, &o).0);
        o.caches = true;
        assert!(classify_volume("tracon-cache-abc", None, &o).0);
        assert!(classify_volume("tracon-provider-anthropic", None, &o).0);
    }

    #[test]
    fn a_check_volume_goes_once_its_run_is_over() {
        let o = owners();
        assert!(!classify_volume("tracon-check-live", None, &o).0);
        assert!(classify_volume("tracon-check-done", None, &o).0);
    }

    #[test]
    fn host_directories_follow_their_owners() {
        let o = owners();
        assert_eq!(
            classify_directory("sessions", "open", None, &o).map(|v| v.0),
            Some(false)
        );
        assert_eq!(
            classify_directory("sessions", "archived", None, &o).map(|v| v.0),
            Some(true)
        );
        assert_eq!(
            classify_directory("workspaces", "shared", Some(0), &o).map(|v| v.0),
            Some(false)
        );
        assert_eq!(
            classify_directory("workspace-staging", "archived", Some(0), &o).map(|v| v.0),
            Some(true)
        );
        assert_eq!(
            classify_directory("opencode-state", "archived", None, &o),
            None
        );
    }
}
