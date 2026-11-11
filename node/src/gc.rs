//! Reclaiming runtime storage whose owner is over.
//!
//! The node creates runtime volumes on demand and, apart from a check run's,
//! never removes them itself: a session's workspace outlives it for resume
//! and export, and its harness state for backup and restore. Once nothing can
//! use one, it is only disk. A sweep names every volume of this node's and
//! the host directories that shadow them, says for each whether its owner is
//! over and why, and removes only when asked to. This node's volumes are the
//! ones named with its `[boundary] resource_prefix` (`tracon` unless set,
//! which the names below assume); another node sharing the container host
//! names its own differently, and the sweep never lists them.
//!
//! Ownership is read from the name. `tracon-scratch-<session>` belongs to a
//! session; `tracon-workspace-<id>` to every open session using that
//! workspace; `tracon-cache-w-<id>` to that same workspace, whose sessions
//! installed into it before session caches were kept per repository;
//! `tracon-check-<run>` to a check run, as `tracon-prep-…`, `tracon-warm-…`,
//! a setup trial's `tracon-try-…` and a `fetch_branch`'s bundle
//! `tracon-fetch-…` are to the run that made them;
//! `tracon-cache-<hash>` — a repository's base cache, or as
//! `tracon-cache-r-<hash>` its sessions' build cache — to nobody (it is
//! rebuilt on demand, so it goes only when asked for). The
//! node-wide volumes are never candidates, and neither is a name this node
//! does not recognize. A publication's working repository under
//! `publishers/`, and its bundle beside it, belong to that publication until
//! it is settled.
//!
//! Where sandboxes run from the bundle: a harness bundle under
//! `bundles/` belongs to the node's version, and one of any other version is
//! never mounted again; a repository's prepared snapshot (an image) belongs
//! to the repository while it is the latest one prepared, and is otherwise
//! superseded.
//!
//! A container a copy into or out of a volume made (`tracon-transfer-…`)
//! belongs to that copy, which removes it when it ends; one a day old was
//! left by a copy that never finished, and goes first, so the image and the
//! volume it holds as in use go in the same sweep. A workspace's directory
//! goes only with its volume: when the volume could not be removed, the
//! directory stays beside it.
//!
//! Images on Podman and in Apple's `container` store are swept by
//! `crate::image_gc`: only those carrying this node's owner labels, by class
//! (superseded bases, repository images, snapshots, dangling builds, and with
//! `--legacy` what sandbox images retired), with what each would free.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::boundary::Backend;
use crate::config::Config;
use crate::store::Store;

/// A workspace nothing references yet may be about to be: a browser import
/// is referenced only once a session starts on it.
pub const WORKSPACE_GRACE_MS: i64 = 24 * 60 * 60 * 1000;

/// What `session::materialize` names the volumes every session shares,
/// after the prefix.
const NODE_WIDE: [&str; 2] = ["harness-state", "probe-scratch"];

/// A volume name less this node's prefix and its `-`, or `None` when the
/// volume is not named for a node with `prefix`.
fn own_rest<'a>(name: &'a str, prefix: &str) -> Option<&'a str> {
    name.strip_prefix(prefix)?.strip_prefix('-')
}

/// Whether a runtime volume is named for this node.
pub fn is_own(name: &str) -> bool {
    own_rest(name, crate::config::resource_prefix()).is_some()
}

/// What a sweep knows about the owners of runtime storage.
#[derive(Debug)]
pub struct Owners {
    /// This node's `[boundary] resource_prefix`: a volume without it is
    /// another node's.
    pub prefix: String,
    /// Sessions on this node that are not archived.
    pub open_sessions: HashSet<String>,
    /// Every session this node has run.
    pub known_sessions: HashSet<String>,
    /// Workspace ids, as their volume names carry them, that an open session
    /// or a retryable import still uses.
    pub used_workspaces: HashSet<String>,
    pub running_checks: HashSet<String>,
    /// Publications neither opened nor failed, whose recovery may still look.
    pub unsettled_publications: HashSet<String>,
    pub now_ms: i64,
    /// Dependency caches are candidates too.
    pub caches: bool,
    /// The snapshot each repository runs now (`name@sha256:…`).
    pub snapshots: HashSet<String>,
    /// The version whose harness bundle this node mounts.
    pub version: String,
    /// Images this node's current builds, snapshots and open sessions name.
    pub kept_images: Vec<crate::image_gc::Kept>,
    /// Repositories with builds recorded that no `[[repo]]` entry names and
    /// that are no managed clone: nothing builds or prepares for them again,
    /// so their last builds and snapshots are superseded rather than current.
    pub retired_repositories: HashSet<String>,
}

impl Default for Owners {
    fn default() -> Self {
        Owners {
            prefix: crate::config::DEFAULT_RESOURCE_PREFIX.into(),
            open_sessions: HashSet::new(),
            known_sessions: HashSet::new(),
            used_workspaces: HashSet::new(),
            running_checks: HashSet::new(),
            unsettled_publications: HashSet::new(),
            now_ms: 0,
            caches: false,
            snapshots: HashSet::new(),
            version: String::new(),
            kept_images: Vec::new(),
            retired_repositories: HashSet::new(),
        }
    }
}

impl Owners {
    /// `repos` is the `[[repo]]` table as it stands now.
    pub fn read(
        store: &Store,
        node_id: &str,
        caches: bool,
        repos: &[crate::config::Repo],
    ) -> Result<Self, crate::store::StoreError> {
        let mut owners = Owners {
            prefix: crate::config::resource_prefix().into(),
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
        owners.unsettled_publications = store.publications_unsettled()?;
        owners.version = env!("CARGO_PKG_VERSION").to_string();
        let rows = store.repo_images()?;
        owners.retired_repositories = rows
            .iter()
            .map(|row| row.repo_path.clone())
            .filter(|repo| !repository_is_current(repos, Path::new(repo)))
            .collect();
        // The latest ready snapshot of each repository the node still
        // builds for; rows are newest first within a repository.
        let mut latest = HashSet::new();
        let mut rows: Vec<_> = rows
            .into_iter()
            .filter(|row| row.kind == crate::sandbox_images::SNAPSHOT && row.status == "ready")
            .filter(|row| !owners.retired_repositories.contains(&row.repo_path))
            .collect();
        rows.sort_by_key(|row| std::cmp::Reverse(row.started_ms));
        for row in rows {
            if latest.insert(row.repo_path.clone()) {
                owners.snapshots.insert(row.image);
            }
        }
        owners.kept_images = kept_images(store, &owners)?;
        Ok(owners)
    }
}

/// Whether the node still builds for `repo`: a `[[repo]]` entry names it, or
/// it is a managed clone.
fn repository_is_current(repos: &[crate::config::Repo], repo: &Path) -> bool {
    repos.iter().any(|entry| entry.matches(repo)) || crate::forge::managed_clone(repo).is_ok()
}

/// What a repository's work runs in now: the newest finished build of each
/// kind (a stale one still stands while its successor fails) and what it
/// was built on, the current snapshots, and the images open sessions
/// started from. A repository the node no longer builds for has no current
/// build, save the newest trial an entry not yet approved would adopt.
fn kept_images(
    store: &Store,
    owners: &Owners,
) -> Result<Vec<crate::image_gc::Kept>, crate::store::StoreError> {
    use crate::image_gc::Kept;
    let mut kept = Vec::new();
    let repos: std::collections::BTreeSet<String> = store
        .repo_images()?
        .into_iter()
        .map(|row| row.repo_path)
        .collect();
    for repo in repos {
        // A setup trial builds for a repository before its entry exists, and
        // the entry the operator approves adopts that build, so a repository
        // no entry names yet still keeps its newest trial.
        let retired = owners.retired_repositories.contains(&repo);
        let mut kinds = HashSet::new();
        for row in store.ready_repo_images(&repo)? {
            // A session layer (`session:<harness>`) is from before sessions
            // ran from the bundle: nothing runs in it, so it keeps nothing.
            if row.kind == crate::sandbox_images::SNAPSHOT
                || row.kind.starts_with("session:")
                || (retired && row.kind != crate::repo_image::TRIAL)
                || !kinds.insert(row.kind.clone())
            {
                continue;
            }
            let why = format!("the current {} build of {repo}", row.kind);
            for reference in [&row.image, &row.parent_image] {
                if !reference.is_empty() {
                    kept.push(Kept {
                        reference: reference.clone(),
                        why: why.clone(),
                    });
                }
            }
        }
    }
    for snapshot in &owners.snapshots {
        kept.push(Kept {
            reference: snapshot.clone(),
            why: "a repository's current prepared snapshot".into(),
        });
    }
    for session in &owners.open_sessions {
        let Some(started) = store.last_event_payload(session, "session_started")? else {
            continue;
        };
        for field in ["image", "toolchain_image"] {
            if let Some(image) = started[field].as_str().filter(|i| !i.is_empty()) {
                kept.push(Kept {
                    reference: image.to_string(),
                    why: format!("session {session} is open and started from it"),
                });
            }
        }
    }
    Ok(kept)
}

/// A workspace id as its volume name and host directories carry it.
fn workspace_key(id: &str) -> String {
    let volume = crate::workspace::volume_name(id);
    volume
        .strip_prefix(&crate::config::resource_name("workspace-"))
        .unwrap_or(&volume)
        .to_string()
}

/// One piece of runtime storage and what the sweep makes of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Item {
    /// `container` (a transfer's), `volume`, `directory`, `file` (a
    /// publication's bundle), or `image`.
    pub kind: &'static str,
    /// The volume name, or the directory relative to the node's state dir.
    pub name: String,
    pub remove: bool,
    pub reason: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub removed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// An image's class (`crate::image_gc::Class`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<&'static str>,
    /// About what removing an image frees, in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
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
    let Some(name) = own_rest(name, &owners.prefix) else {
        return verdict(false, "not named for this node; another node may own it");
    };
    if NODE_WIDE.contains(&name) {
        return verdict(false, "node-wide; every session uses it");
    }
    if let Some(session) = name.strip_prefix("scratch-") {
        return session_verdict(
            session,
            owners,
            "its harness state can no longer be backed up, upgraded or restored",
        );
    }
    if let Some(key) = name.strip_prefix("workspace-") {
        return workspace_verdict(key, created_ms, owners);
    }
    if let Some(run) = name.strip_prefix("check-") {
        return if owners.running_checks.contains(run) {
            verdict(false, "the check is running")
        } else {
            verdict(true, "the check run has finished")
        };
    }
    // What one check run, one cache warm-up or one fetch_branch made for
    // itself, and removes itself when it ends. One still here a day later was
    // left by a run that did not finish.
    if ["prep-", "warm-", "try-", "fetch-"]
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
    if let Some(key) = name.strip_prefix("cache-w-") {
        return workspace_verdict(key, created_ms, owners);
    }
    if name.starts_with("cache-r-") {
        return if owners.caches {
            verdict(true, "session build cache; the next session rebuilds it")
        } else {
            verdict(
                false,
                "session build cache; included only when caches are asked for",
            )
        };
    }
    if name.starts_with("cache-") {
        return if owners.caches {
            verdict(true, "dependency cache; rebuilt when next needed")
        } else {
            verdict(
                false,
                "dependency cache; included only when caches are asked for",
            )
        };
    }
    if name.starts_with("provider-") {
        return verdict(
            true,
            "a sign-in container's volume; providers sign in natively now",
        );
    }
    verdict(false, "not a volume this node recognizes")
}

/// Whether a transfer's container is over: one still copying (running, or
/// made in the last day, as a copy holds a stopped container between its
/// steps) stays.
pub fn classify_transfer(
    transfer: &crate::boundary::TransferInfo,
    owners: &Owners,
) -> (bool, String) {
    if own_rest(&transfer.name, &owners.prefix).is_none() {
        return verdict(false, "not named for this node; another node may own it");
    }
    if transfer.running {
        return verdict(false, "running; a copy is under way");
    }
    if transfer
        .created_ms
        .is_none_or(|created| owners.now_ms - created < WORKSPACE_GRACE_MS)
    {
        return verdict(
            false,
            "made in the last day; its copy may still be under way",
        );
    }
    verdict(
        true,
        "left by a copy that did not finish; it holds its image and volume as in use",
    )
}

/// The volume a workspace's host directory is the shadow of, under `prefix`:
/// the two go together or not at all.
fn workspace_volume(item: &Item, prefix: &str) -> Option<String> {
    let key = item
        .name
        .strip_prefix("workspaces/")
        .or_else(|| item.name.strip_prefix("workspace-staging/"))?;
    Some(format!("{prefix}-workspace-{key}"))
}

/// Keep a workspace's directory whose volume this sweep could not remove.
fn hold_with_volume(item: &mut Item, failed_volumes: &HashSet<String>, prefix: &str) {
    if let Some(volume) = workspace_volume(item, prefix).filter(|v| failed_volumes.contains(v)) {
        item.remove = false;
        item.reason = format!("its volume {volume} could not be removed, so it stays with it");
    }
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
        "bundles" => Some(match entry {
            crate::service_bundle::DIR => verdict(
                false,
                "the release's browser bundles; the node removes other Chrome versions itself",
            ),
            version if version == owners.version => {
                verdict(false, "the harness bundle this node mounts")
            }
            incoming if incoming.starts_with(".incoming-") => {
                verdict(true, "an interrupted download of a harness bundle")
            }
            _ => verdict(
                true,
                "a harness bundle of another version; this node never mounts it",
            ),
        }),
        // Every attempt makes both afresh from the candidate, so once the
        // publication is settled they serve nothing.
        "publishers" => {
            let id = entry.strip_suffix(".bundle").unwrap_or(entry);
            Some(match owners.unsettled_publications.contains(id) {
                true => verdict(
                    false,
                    format!("publication {id} is not settled; its recovery may still look"),
                ),
                false => verdict(
                    true,
                    format!("publication {id} is opened, failed or gone; a retry starts afresh"),
                ),
            })
        }
        _ => None,
    }
}

/// Top-level directories no current code reads.
const RETIRED_DIRECTORIES: [(&str, &str); 5] = [
    (
        "providers",
        "sign-in container state; providers sign in natively now",
    ),
    (
        "harness-state",
        "harness state from before it moved into the runtime volume",
    ),
    (
        "opencode-ui",
        "the OpenCode web interface bundle; the OpenCode harness was removed",
    ),
    (
        "opencode-state",
        "backups of OpenCode sessions' state; the OpenCode harness was removed",
    ),
    (
        "opencode-state-work",
        "scratch for reading OpenCode sessions' state; the OpenCode harness was removed",
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
    for area in [
        "sessions",
        "workspaces",
        "workspace-staging",
        "publishers",
        crate::bundle::DIR,
    ] {
        let Ok(entries) = std::fs::read_dir(state_dir.join(area)) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // A publication's bundle is the one file a sweep looks at.
            let bundle = area == "publishers" && path.extension().is_some_and(|e| e == "bundle");
            if !path.is_dir() && !bundle {
                continue;
            }
            let entry = entry.file_name().to_string_lossy().into_owned();
            if let Some((remove, reason)) =
                classify_directory(area, &entry, created_ms(&path), owners)
            {
                let kind = if bundle { "file" } else { "directory" };
                items.push((item(kind, format!("{area}/{entry}"), remove, reason), path));
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
        class: None,
        size: None,
    }
}

/// What a sweep is asked to do.
#[derive(Debug, Clone, Copy, Default)]
pub struct Sweep {
    /// Dependency caches are candidates too.
    pub caches: bool,
    pub apply: bool,
    /// What older nodes made that nothing runs any more (harness images,
    /// layered and superseded `-src` repository images) is a candidate too.
    pub legacy: bool,
    /// Unlabelled images named as this node's are taken as its own.
    pub adopt_unlabelled: bool,
}

/// The legacy images' repositories: every harness image an older node named
/// from `[boundary] harness_image` (as its `node.toml` still says, else the
/// default), and its gateway's.
fn legacy_repositories(cfg: &Config) -> Vec<String> {
    use crate::image_gc::repository_of;
    let configured = repository_of(&cfg.retired.harness_image).to_string();
    let stem = configured
        .strip_suffix(&format!("-{}", cfg.harness.id))
        .filter(|stem| !stem.is_empty())
        .unwrap_or("localhost/tracon-harness")
        .to_string();
    let mut out = vec![configured];
    for id in ["claude", "native", "opencode"] {
        out.push(format!("{stem}-{id}"));
    }
    out.push(repository_of(&cfg.retired.gateway_image).to_string());
    out.sort();
    out.dedup();
    out
}

/// What the image sweep knows of this node.
pub fn image_knowledge(
    cfg: &Config,
    owners: &Owners,
    node_id: &str,
    sweep: &Sweep,
) -> crate::image_gc::Knowledge {
    crate::image_gc::Knowledge {
        prefix: owners.prefix.clone(),
        node_id: node_id.to_string(),
        base_repository: Some(
            crate::image_gc::repository_of(&cfg.sandbox.base_image_ref()).to_string(),
        ),
        pinned_base: crate::sandbox_images::active_base(),
        bases_are_ours: owners.prefix == crate::config::DEFAULT_RESOURCE_PREFIX,
        kept: owners.kept_images.clone(),
        legacy_repositories: legacy_repositories(cfg),
        now_ms: owners.now_ms,
        legacy: sweep.legacy,
        adopt_unlabelled: sweep.adopt_unlabelled,
        departing: Vec::new(),
    }
}

/// The transfers' containers, and with `apply` those whose copy is over
/// removed; with them, the images of those that go and are not removed yet,
/// which an image sweep counts a container fewer. A runtime that cannot list
/// them has none to offer.
async fn sweep_transfers(
    backend: &dyn Backend,
    owners: &Owners,
    apply: bool,
) -> (Vec<Item>, Vec<String>) {
    let transfers = match backend.list_transfers().await {
        Ok(transfers) => transfers,
        Err(error) => {
            tracing::debug!(%error, "could not list transfer containers");
            return (Vec::new(), Vec::new());
        }
    };
    let mut items = Vec::new();
    let mut departing = Vec::new();
    for transfer in transfers {
        let (remove, reason) = classify_transfer(&transfer, owners);
        let mut item = item("container", transfer.name.clone(), remove, reason);
        if remove && apply {
            match backend.remove_transfer(&transfer.name).await {
                Ok(()) => item.removed = true,
                Err(error) => item.error = Some(error.to_string()),
            }
        }
        if remove && !apply && !transfer.image.is_empty() {
            departing.push(transfer.image);
        }
        items.push(item);
    }
    (items, departing)
}

/// The image store `backend` keeps, swept.
async fn sweep_images(
    backend: &dyn Backend,
    cfg: &Config,
    knowledge: &crate::image_gc::Knowledge,
    apply: bool,
) -> Result<Vec<crate::image_gc::Swept>, String> {
    match backend.kind() {
        "apple" => {
            let cli = crate::provider::apple::SystemCli {
                bin: cfg.runtime.apple.container.clone(),
            };
            crate::image_gc::apple::sweep(&cli, knowledge, apply).await
        }
        _ => crate::image_gc::podman::sweep(knowledge, apply).await,
    }
}

/// Say in the node's log how much of the image store is reclaimable, without
/// removing anything: run after the boundary converges.
pub async fn note_reclaimable_images(
    backend: &dyn Backend,
    cfg: &Config,
    store: &Store,
    node_id: &str,
) {
    if !matches!(backend.kind(), "podman" | "apple") {
        return;
    }
    let owners = match Owners::read(store, node_id, false, &cfg.repos()) {
        Ok(owners) => owners,
        Err(error) => {
            tracing::debug!(%error, "could not read what this node's images serve");
            return;
        }
    };
    let mut knowledge = image_knowledge(cfg, &owners, node_id, &Sweep::default());
    let (_, departing) = sweep_transfers(backend, &owners, false).await;
    knowledge.departing = departing;
    match sweep_images(backend, cfg, &knowledge, false).await {
        Ok(swept) => log_reclaimable(&swept, false),
        Err(error) => tracing::debug!(%error, "could not list the image store"),
    }
}

fn log_reclaimable(swept: &[crate::image_gc::Swept], applied: bool) {
    let going: Vec<_> = swept.iter().filter(|s| s.remove).collect();
    if going.is_empty() {
        return;
    }
    let bytes: u64 = going.iter().map(|s| s.reclaim).sum();
    let mut classes: Vec<&str> = going.iter().map(|s| s.class.as_str()).collect();
    classes.dedup();
    let classes = classes.join(",");
    match applied {
        true => tracing::info!(
            images = going.len(),
            bytes,
            %classes,
            "removed reclaimable images"
        ),
        false => tracing::info!(
            images = going.len(),
            bytes,
            %classes,
            "images are reclaimable; `tracon gc` lists them and `tracon gc --apply` removes them"
        ),
    }
}

/// Name every piece of runtime storage and, with `apply`, remove the ones
/// whose owner is over. A volume a harness still mounts is refused by the
/// backend and reported, not forced.
pub async fn sweep(
    backend: &dyn Backend,
    cfg: &Config,
    store: &Store,
    node_id: &str,
    sweep: Sweep,
) -> Result<Vec<Item>, String> {
    let Sweep { caches, apply, .. } = sweep;
    let owners = Owners::read(store, node_id, caches, &cfg.repos()).map_err(|e| e.to_string())?;
    // Transfers first: one left behind holds its volume and its image.
    let (mut items, departing) = sweep_transfers(backend, &owners, apply).await;
    let volumes = backend.list_volumes().await.map_err(|e| e.to_string())?;
    let mut failed_volumes = HashSet::new();
    for volume in volumes {
        let (remove, reason) = classify_volume(&volume.name, volume.created_ms, &owners);
        let mut item = item("volume", volume.name, remove, reason);
        if apply && remove {
            let lock = crate::workspace::volume_lock(&item.name);
            let _guard = lock.lock().await;
            match backend.remove_volume(&item.name).await {
                Ok(()) => item.removed = true,
                Err(error) => {
                    failed_volumes.insert(item.name.clone());
                    item.error = Some(error.to_string());
                }
            }
        }
        items.push(item);
    }
    if matches!(backend.kind(), "podman" | "apple") {
        let mut knowledge = image_knowledge(cfg, &owners, node_id, &sweep);
        knowledge.departing = departing;
        let swept = sweep_images(backend, cfg, &knowledge, apply).await?;
        log_reclaimable(&swept, apply);
        for image in swept {
            items.push(Item {
                removed: image.removed,
                error: image.error,
                class: Some(image.class.as_str()),
                size: Some(image.reclaim),
                ..item("image", image.name, image.remove, image.reason)
            });
        }
    }
    for (mut item, path) in directory_items(&Config::state_dir(), &owners) {
        hold_with_volume(&mut item, &failed_volumes, &owners.prefix);
        if apply && item.remove {
            let removed = match path.is_dir() {
                true => std::fs::remove_dir_all(&path),
                false => std::fs::remove_file(&path),
            };
            match removed {
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
        owners.unsettled_publications.insert("pending".into());
        owners
    }

    #[test]
    fn only_the_current_bundle_stays() {
        let mut o = owners();
        o.version = "1.2.0".into();
        assert!(!classify_directory("bundles", "1.2.0", None, &o).unwrap().0);
        assert!(classify_directory("bundles", "1.1.0", None, &o).unwrap().0);
        assert!(
            !classify_directory("bundles", "browser", None, &o)
                .unwrap()
                .0
        );
        assert!(
            classify_directory("bundles", ".incoming-x", None, &o)
                .unwrap()
                .0
        );
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
            "tracon-try-0199",
            "tracon-fetch-0199",
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
        // A repository's session build cache outlives every session, so
        // only asking for caches takes it.
        assert!(!classify_volume("tracon-cache-r-abc", None, &o).0);
        o.caches = true;
        assert!(classify_volume("tracon-cache-abc", None, &o).0);
        assert!(classify_volume("tracon-cache-r-abc", None, &o).0);
        assert!(classify_volume("tracon-provider-anthropic", None, &o).0);
    }

    /// A second node on the same container host names its volumes with its
    /// own prefix, and neither node's sweep ever offers the other's.
    #[test]
    fn a_sweep_offers_only_volumes_named_for_this_node() {
        let mut o = owners();
        o.caches = true;
        o.prefix = "spike-s".into();
        for theirs in [
            "tracon-scratch-gone",
            "tracon-workspace-archived",
            "tracon-check-done",
            "tracon-cache-abc",
            "tracon-harness-state",
            "spike-scratch-gone",
            "spike-sx-scratch-gone",
        ] {
            let (remove, reason) = classify_volume(theirs, None, &o);
            assert!(!remove, "{theirs}");
            assert!(reason.contains("not named for this node"), "{reason}");
        }
        assert!(classify_volume("spike-s-scratch-gone", None, &o).0);
        assert!(classify_volume("spike-s-check-done", None, &o).0);
        assert!(!classify_volume("spike-s-harness-state", None, &o).0);
        assert!(!classify_volume("spike-s-probe-scratch", None, &o).0);
        // And the default node does not take the other's for its own.
        let o = owners();
        assert!(!classify_volume("spike-s-scratch-gone", None, &o).0);
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

    /// A publication's working repository and bundle are made afresh by every
    /// attempt, so they stay only while a recovery may still look at them.
    #[test]
    fn a_publishers_repository_and_bundle_go_once_the_publication_is_settled() {
        let o = owners();
        for entry in ["pending", "pending.bundle"] {
            assert_eq!(
                classify_directory("publishers", entry, None, &o).map(|v| v.0),
                Some(false)
            );
        }
        for entry in ["opened", "opened.bundle"] {
            assert_eq!(
                classify_directory("publishers", entry, None, &o).map(|v| v.0),
                Some(true)
            );
        }
    }

    /// A transfer's container outlives its copy only when the node stopped
    /// mid-copy; one a day old goes, one copying now stays.
    #[test]
    fn a_transfer_s_container_goes_once_its_copy_is_over() {
        let o = owners();
        let transfer =
            |name: &str, running: bool, created_ms: Option<i64>| crate::boundary::TransferInfo {
                name: name.into(),
                running,
                created_ms,
                image: "harness".into(),
            };
        let (remove, reason) =
            classify_transfer(&transfer("tracon-transfer-01a1", false, Some(0)), &o);
        assert!(remove, "{reason}");
        assert!(!classify_transfer(&transfer("tracon-transfer-01a1", true, Some(0)), &o).0);
        let recent = Some(o.now_ms - 1000);
        assert!(!classify_transfer(&transfer("tracon-transfer-01a1", false, recent), &o).0);
        assert!(!classify_transfer(&transfer("tracon-transfer-01a1", false, None), &o).0);
        assert!(!classify_transfer(&transfer("spike-transfer-01a1", false, Some(0)), &o).0);
    }

    /// A workspace's directory and its volume go together: when the volume
    /// could not be removed, its directory stays, and others are untouched.
    #[test]
    fn a_workspace_directory_stays_with_a_volume_that_could_not_go() {
        let failed: HashSet<String> = ["tracon-workspace-01a14200".to_string()].into();
        let directory = |name: &str| item("directory", name.into(), true, "no open session".into());
        for held in ["workspaces/01a14200", "workspace-staging/01a14200"] {
            let mut it = directory(held);
            hold_with_volume(&mut it, &failed, "tracon");
            assert!(!it.remove, "{held}");
            assert!(
                it.reason.contains("tracon-workspace-01a14200"),
                "{}",
                it.reason
            );
        }
        for free in ["workspaces/01a14201", "sessions/01a14200"] {
            let mut it = directory(free);
            hold_with_volume(&mut it, &failed, "tracon");
            assert!(it.remove, "{free}");
        }
    }

    /// A build of a repository no `[[repo]]` entry names any more (and that
    /// is no managed clone) is superseded: it keeps neither its image, nor
    /// what that was built on, nor its snapshot.
    #[test]
    fn a_build_of_a_repository_the_node_no_longer_names_is_superseded() {
        let store = Store::open_in_memory().unwrap();
        let built = |repo: &str, kind: &str, image: &str, parent: &str| {
            let id = store
                .start_repo_image(&crate::store::RepoImageStart {
                    repo_path: repo,
                    kind,
                    recipe_hash: "r",
                    source_ref: "",
                    source_commit: "c",
                    parent_image: parent,
                })
                .unwrap();
            store.finish_repo_image(&id, Ok(image), "", &[]).unwrap();
        };
        built(
            "/src/kept",
            crate::repo_image::BASE,
            "localhost/kept:1",
            "localhost/kept-src:1",
        );
        built(
            "/src/gone",
            crate::repo_image::BASE,
            "localhost/gone:1",
            "localhost/gone-src:1",
        );
        built(
            "/src/kept",
            crate::sandbox_images::SNAPSHOT,
            "localhost/kept-snap:1",
            "",
        );
        built(
            "/src/gone",
            crate::sandbox_images::SNAPSHOT,
            "localhost/gone-snap:1",
            "",
        );
        built(
            "/src/trying",
            crate::repo_image::TRIAL,
            "localhost/trying:1",
            "localhost/trying-src:1",
        );
        let repos = vec![crate::config::Repo {
            path: "/src/kept".into(),
            ..Default::default()
        }];
        assert!(repository_is_current(&repos, Path::new("/src/kept")));
        assert!(!repository_is_current(&repos, Path::new("/src/gone")));

        let o = Owners::read(&store, "node-a", false, &repos).unwrap();
        assert_eq!(
            o.retired_repositories,
            HashSet::from(["/src/gone".to_string(), "/src/trying".to_string()])
        );
        assert_eq!(
            o.snapshots,
            HashSet::from(["localhost/kept-snap:1".to_string()])
        );
        let kept: Vec<&str> = o.kept_images.iter().map(|k| k.reference.as_str()).collect();
        assert_eq!(
            kept,
            [
                "localhost/kept:1",
                "localhost/kept-src:1",
                "localhost/trying:1",
                "localhost/trying-src:1",
                "localhost/kept-snap:1"
            ]
        );
    }
}
