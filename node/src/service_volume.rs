//! A repository's persistent data for a catalogue service: a Postgres
//! cluster or a Redis dump that is there again in the next session.
//!
//! A `[[repo]]` entry's `service_volumes` names the catalogue services
//! (`[[service]]`) whose data that repository keeps. Each gets a volume of
//! its own for the repository, `<prefix>-svcdata-<hash>` (the hash of the
//! repository's path and the service's name), mounted writable at
//! `/var/lib/tracon-services/<name>` in every session's sandbox on that
//! repository. The service's bundle keeps its data there when the directory
//! is mounted (its `run` is told where by `TRACON_SERVICE_DATA`), and in a
//! directory of its own that goes with the sandbox when it is not: a service
//! no entry names starts empty in each session, as it always has.
//!
//! The volume is a tree the node keeps (`runner::sandbox_files`), on every
//! runtime: sent into the sandbox when it starts and taken back only when it
//! ends, after the sandbox's services have stopped, so what comes back is a
//! cleanly stopped database rather than its files copied mid-write. Two
//! sessions on one repository at once each start from the node's copy, and
//! the one that ends last is what the next session finds.
//!
//! Nothing a session does removes one. The operator resets it (Settings ›
//! Data, `tracon repo reset-volume`), which removes the tree: the next
//! session starts the service empty and keeps what it writes from then on.
//! Once no `[[repo]]` entry names its repository, a sweep (`tracon gc`, the
//! `[gc]` schedule) removes it with the repository's builds and snapshots.

use std::path::Path;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::runner::sandbox_files::FilesStore;
use crate::runner::Mount;
use crate::store::{ServiceVolumeRow, Store};

/// Where a service's volume is mounted inside a sandbox, under its name.
pub const TARGET: &str = "/var/lib/tracon-services";

/// What a service's `run` is told its data directory is. Mounted only when
/// the session's repository keeps the service's data; a `run` that finds no
/// directory there keeps its data in one that goes with the sandbox.
pub const DATA_ENV: &str = "TRACON_SERVICE_DATA";

/// Where `service`'s data is inside a sandbox.
pub fn target(service: &str) -> String {
    format!("{TARGET}/{service}")
}

/// The volume holding `repo`'s data for `service`.
pub fn volume_name(repo: &Path, service: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(repo.as_os_str().as_encoded_bytes());
    hash.update([0]);
    hash.update(service.as_bytes());
    crate::config::resource_name(&format!("svcdata-{}", &hex::encode(hash.finalize())[..24]))
}

/// The catalogue services whose data `repo` keeps: those its `[[repo]]`
/// entry names that the catalogue offers, in the entry's order.
pub fn kept_for(cfg: &Config, repo: &Path) -> Vec<String> {
    let repos = cfg.repos();
    let Some(entry) = repos.iter().find(|entry| entry.matches(repo)) else {
        return Vec::new();
    };
    entry
        .service_volumes
        .iter()
        .filter(|name| cfg.service.iter().any(|service| &service.name == *name))
        .cloned()
        .collect()
}

/// The mounts a session on `repo` gets for its services' data, each volume
/// recorded as used now and kept in `files`, so every runtime sends it in.
/// A volume that cannot be kept is left out, and its service starts empty.
pub fn session_mounts(cfg: &Config, store: &Store, files: &FilesStore, repo: &Path) -> Vec<Mount> {
    let mut mounts = Vec::new();
    for service in kept_for(cfg, repo) {
        let volume = volume_name(repo, &service);
        if let Err(error) = files.ensure(&volume) {
            tracing::warn!(%volume, %service, %error, "a service's data volume could not be kept; it starts empty");
            continue;
        }
        if let Err(error) = store.service_volume_used(
            &volume,
            &repo.to_string_lossy(),
            &service,
            crate::store::now_ms(),
        ) {
            tracing::warn!(%volume, %service, %error, "could not record a service's data volume");
        }
        mounts.push(Mount::volume(volume, target(&service), false));
    }
    mounts
}

/// One service volume, as the operator is shown it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Held {
    pub volume: String,
    pub repo: String,
    pub service: String,
    pub created_ms: i64,
    /// When a session last mounted it.
    pub used_ms: i64,
    /// What its files take on this node.
    pub bytes: i64,
    /// A running sandbox holds it: a reset waits for that session to end.
    pub live: bool,
    /// The repository's entry still names the service, so its next session
    /// mounts it.
    pub mounted: bool,
}

/// Every service volume the node keeps, with what each weighs in `files`.
pub fn list(
    cfg: &Config,
    store: &Store,
    files: &FilesStore,
) -> Result<Vec<Held>, crate::store::StoreError> {
    Ok(store
        .service_volumes()?
        .into_iter()
        .map(|row| held(cfg, files, row))
        .collect())
}

fn held(cfg: &Config, files: &FilesStore, row: ServiceVolumeRow) -> Held {
    let bytes = files
        .tree(&row.volume)
        .map(|tree| crate::data::tree_bytes(&tree))
        .unwrap_or(0);
    let mounted = kept_for(cfg, Path::new(&row.repo_path)).contains(&row.service);
    Held {
        live: crate::runner::sandbox_files::is_live(&row.volume),
        volume: row.volume,
        repo: row.repo_path,
        service: row.service,
        created_ms: row.created_ms,
        used_ms: row.used_ms,
        bytes,
        mounted,
    }
}

/// The volume `repo`'s `service` data is in, when the node keeps one: by the
/// repository's path as a session records it, or as the end of it (a
/// managed clone's `owner/name`).
pub fn find(
    store: &Store,
    repo: &str,
    service: &str,
) -> Result<Option<String>, crate::store::StoreError> {
    let rows = store.service_volumes()?;
    let matching: Vec<&ServiceVolumeRow> = rows
        .iter()
        .filter(|row| row.service == service)
        .filter(|row| {
            row.repo_path == repo
                || crate::config::repo_path_matches(Path::new(repo), Path::new(&row.repo_path))
        })
        .collect();
    Ok(match matching.as_slice() {
        [one] => Some(one.volume.clone()),
        _ => matching
            .iter()
            .find(|row| row.repo_path == repo)
            .map(|row| row.volume.clone()),
    })
}

/// Reset a service volume: remove its tree, so the next session starts the
/// service empty. Refused while a running sandbox holds it. The operator's
/// alone: no tool a session has reaches this.
pub async fn reset(
    backend: &dyn crate::boundary::Backend,
    store: &Store,
    files: &FilesStore,
    volume: &str,
) -> Result<(), String> {
    if !crate::runner::sandbox_files::is_service_data(volume) {
        return Err(format!("{volume} is not a service volume"));
    }
    if store
        .service_volume(volume)
        .map_err(|e| e.to_string())?
        .is_none()
    {
        return Err(format!("no service volume {volume} on this node"));
    }
    let lock = crate::workspace::volume_lock(volume);
    let _guard = lock.lock().await;
    if files.holds(volume) {
        crate::boundary::refuse_live(volume).map_err(|error| error.to_string())?;
        files.remove(volume).map_err(|error| error.to_string())?;
    } else {
        // Kept by the runtime, not the node: there is nothing else to say
        // it is gone but the runtime itself.
        backend
            .remove_volume(volume)
            .await
            .map_err(|error| error.to_string())?;
    }
    store
        .forget_service_volume(volume)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Repo, Service};

    fn cfg() -> Config {
        Config {
            service: ["postgres", "redis", "browser"]
                .into_iter()
                .map(|name| Service {
                    name: name.into(),
                    bundle: Some(format!("/srv/{name}").into()),
                    port: 1,
                    ..Service::default()
                })
                .collect(),
            repo: vec![Repo {
                path: "/src/app".into(),
                // `mailpit` is in no catalogue: nothing is mounted for it.
                service_volumes: vec!["redis".into(), "postgres".into(), "mailpit".into()],
                ..Repo::default()
            }],
            ..Config::default()
        }
    }

    #[test]
    fn a_volume_is_the_repositorys_and_the_services_alone() {
        let a = volume_name(Path::new("/src/app"), "postgres");
        assert!(
            a.starts_with(&crate::config::resource_name("svcdata-")),
            "{a}"
        );
        assert!(crate::runner::sandbox_files::is_service_data(&a));
        assert!(crate::runner::sandbox_files::backs(&a));
        assert_eq!(a, volume_name(Path::new("/src/app"), "postgres"));
        assert_ne!(a, volume_name(Path::new("/src/app"), "redis"));
        assert_ne!(a, volume_name(Path::new("/src/other"), "postgres"));
    }

    #[test]
    fn only_the_entrys_catalogue_services_are_mounted_and_recorded() {
        let cfg = cfg();
        let store = Store::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let files = FilesStore::new(dir.path());
        let repo = Path::new("/src/app");
        assert_eq!(kept_for(&cfg, repo), ["redis", "postgres"]);
        assert!(kept_for(&cfg, Path::new("/src/elsewhere")).is_empty());
        let mounts = session_mounts(&cfg, &store, &files, repo);
        let targets: Vec<&str> = mounts.iter().map(|m| m.target.as_str()).collect();
        assert_eq!(
            targets,
            [
                "/var/lib/tracon-services/redis",
                "/var/lib/tracon-services/postgres"
            ]
        );
        assert!(mounts.iter().all(|m| !m.read_only));
        // Each is a tree the node keeps, so every runtime sends it in.
        for mount in &mounts {
            assert!(files.holds(&mount.volume), "{}", mount.volume);
        }
        let held = list(&cfg, &store, &files).unwrap();
        assert_eq!(held.len(), 2);
        assert!(held
            .iter()
            .all(|h| h.mounted && !h.live && h.repo == "/src/app"));
        // A session with no entry gets nothing.
        assert!(session_mounts(&cfg, &store, &files, Path::new("/src/elsewhere")).is_empty());
        // Found by the repository's path, or by the end of it.
        let postgres = volume_name(repo, "postgres");
        assert_eq!(
            find(&store, "/src/app", "postgres").unwrap(),
            Some(postgres.clone())
        );
        assert_eq!(find(&store, "app", "postgres").unwrap(), Some(postgres));
        assert_eq!(find(&store, "/src/app", "browser").unwrap(), None);
    }

    #[test]
    fn a_volume_whose_entry_dropped_the_service_is_listed_as_not_mounted() {
        let mut cfg = cfg();
        let store = Store::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let files = FilesStore::new(dir.path());
        session_mounts(&cfg, &store, &files, Path::new("/src/app"));
        std::fs::write(
            files
                .tree(&volume_name(Path::new("/src/app"), "redis"))
                .unwrap()
                .join("dump.rdb"),
            [0u8; 64],
        )
        .unwrap();
        cfg.repo[0].service_volumes = vec!["postgres".into()];
        let held = list(&cfg, &store, &files).unwrap();
        let redis = held.iter().find(|h| h.service == "redis").unwrap();
        assert!(!redis.mounted);
        assert_eq!(redis.bytes, 64);
    }
}
