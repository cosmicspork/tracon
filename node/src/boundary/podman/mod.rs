//! The rootless Podman boundary, on a Linux host: every harness, check and
//! preparation in a sandbox container of its own, under `tracon-sandbox`
//! from the harness bundle, over the sandbox protocol. A sandbox has no
//! network (`--network none`): its one way out is the node's socket, and the
//! MCP and model gateway, the allowlist proxy and the per-client egress proxy
//! are tunnels over that connection, on the sandbox's loopback. Its
//! workspace and harness state are on its own disk, sent in and read back
//! over the files channel; it runs the stock base or a repository's image
//! built on it.

pub mod checks;
pub mod setup;

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;

use super::{Backend, BoundaryError, BoundaryReport};
use crate::config::Config;
use crate::runner::podman::{PodmanRunner, RunSpec};
use crate::runner::{Mount, Runner};

async fn volume_copy_in(
    podman_bin: &str,
    (image, user): (&str, Option<&str>),
    volume: &str,
    source: &Path,
) -> Result<(), BoundaryError> {
    let podman_bin = &guarded_podman(podman_bin)?;
    crate::workspace::validate_tree(source).map_err(|e| BoundaryError::Other(e.to_string()))?;
    let created = podman_at(podman_bin)
        .args(["volume", "create", "--ignore", volume])
        .output()
        .await
        .map_err(BoundaryError::Io)?;
    if !created.status.success() {
        return Err(BoundaryError::Other(format!(
            "create runtime volume {volume}: {}",
            String::from_utf8_lossy(&created.stderr).trim()
        )));
    }
    let name = crate::config::resource_name(&format!("transfer-{}", uuid::Uuid::now_v7().simple()));
    let created = podman_at(podman_bin)
        .args(["create", "--name", &name])
        .args(user_args(user))
        .args([
            "--mount",
            &format!("type=volume,src={volume},dst=/data"),
            image,
            "true",
        ])
        .output()
        .await
        .map_err(BoundaryError::Io)?;
    if !created.status.success() {
        return Err(BoundaryError::Other(format!(
            "create runtime transfer: {}",
            String::from_utf8_lossy(&created.stderr).trim()
        )));
    }
    let copied = podman_at(podman_bin)
        .args([
            "cp",
            &format!("{}/.", source.display()),
            &format!("{name}:/data"),
        ])
        .output()
        .await
        .map_err(BoundaryError::Io);
    let _ = podman_at(podman_bin)
        .args(["rm", "-f", &name])
        .output()
        .await;
    let copied = copied?;
    if copied.status.success() {
        Ok(())
    } else {
        Err(BoundaryError::Other(format!(
            "copy into runtime volume: {}",
            String::from_utf8_lossy(&copied.stderr).trim()
        )))
    }
}

/// Stages a workspace as Git sees it under `/tmp/tracon-export`, in the
/// transfer container's own filesystem: the harness image may be any image,
/// its `/` may be read-only even to root once capabilities are dropped
/// (Fedora ships it 0555), and `/tmp` is writable in all of them. The index
/// names the tracked files and the ignore rules decide the untracked ones, so
/// a deleted file stays deleted and build output stays behind. The list and the archive are files rather than a
/// pipe: a failure on either side of a pipe would stage a partial tree as if
/// it were the whole one. A volume with no repository is copied as it is.
const STAGE_WORKSPACE: &str = r#"set -eu
mkdir -p /tmp/tracon-export
cd /data
if [ -d .git ]; then
  export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
  git -c safe.directory=/data -c core.fsmonitor=false -c core.hooksPath=/dev/null \
    ls-files -z -co --exclude-standard > /tmp/tracon-export.list
  tar --null --ignore-failed-read -T /tmp/tracon-export.list -cf /tmp/tracon-export.tar
  tar -xf /tmp/tracon-export.tar -C /tmp/tracon-export
  rm -f /tmp/tracon-export.tar /tmp/tracon-export.list
  cp -a .git /tmp/tracon-export/.git
else
  cp -a /data/. /tmp/tracon-export/
fi
"#;

/// Copies one volume's tree into another, in a container of the harness
/// image, which may be any image an operator names, so the flags are ones GNU
/// coreutils and busybox both take. `-dRp` is `-a` without its `context` and
/// `xattr`: GNU `cp -a` copies `security.selinux` as an extended attribute
/// even under `--no-preserve=context`, and setting the source's label on the
/// copy is a `relabelto` a confined container is denied, logged once per file.
/// The destination volume already carries the label it should have.
const CLONE_TREE: &str = "cp -dRp --reflink=auto /from/. /to/";

/// Export a workspace volume without what its repository ignores. The
/// repository's own configuration is the agent's to write, so Git reads it
/// only here: in a container with no network, no capabilities and the volume
/// read-only, whose output is validated like any other export.
async fn workspace_copy_out(
    podman_bin: &str,
    (image, user): (&str, Option<&str>),
    volume: &str,
    destination: &Path,
) -> Result<(), BoundaryError> {
    let podman_bin = &guarded_podman(podman_bin)?;
    let name = crate::config::resource_name(&format!("transfer-{}", uuid::Uuid::now_v7().simple()));
    let staged = podman_at(podman_bin)
        .args(["run", "--name", &name])
        .args(user_args(user))
        .args([
            "--network",
            "none",
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--entrypoint",
            "sh",
            "--mount",
            &format!("type=volume,src={volume},dst=/data,ro"),
            image,
            "-c",
            STAGE_WORKSPACE,
        ])
        .output()
        .await
        .map_err(BoundaryError::Io);
    let result = match staged {
        Ok(staged) if staged.status.success() => {
            copy_out(
                podman_bin,
                &format!("{name}:/tmp/tracon-export/."),
                destination,
            )
            .await
        }
        Ok(staged) => Err(BoundaryError::Other(format!(
            "stage workspace export: {}",
            String::from_utf8_lossy(&staged.stderr).trim()
        ))),
        Err(error) => Err(error),
    };
    let _ = podman_at(podman_bin)
        .args(["rm", "-f", &name])
        .output()
        .await;
    result?;
    crate::workspace::validate_tree(destination).map_err(|e| BoundaryError::Other(e.to_string()))
}

async fn copy_out(podman_bin: &str, source: &str, destination: &Path) -> Result<(), BoundaryError> {
    let podman_bin = &guarded_podman(podman_bin)?;
    if destination.exists() {
        std::fs::remove_dir_all(destination)?;
    }
    // `podman cp <container>:<dir>/. <dest>` copies into `<dest>`'s parent when
    // `<dest>` does not exist yet; it must exist first to receive the tree.
    std::fs::create_dir_all(destination)?;
    let copied = podman_at(podman_bin)
        .args(["cp", source, &destination.to_string_lossy()])
        .output()
        .await
        .map_err(BoundaryError::Io)?;
    if copied.status.success() {
        Ok(())
    } else {
        Err(BoundaryError::Other(format!(
            "copy out of runtime volume: {}",
            String::from_utf8_lossy(&copied.stderr).trim()
        )))
    }
}

async fn volume_copy_out(
    podman_bin: &str,
    (image, user): (&str, Option<&str>),
    volume: &str,
    destination: &Path,
) -> Result<(), BoundaryError> {
    let podman_bin = &guarded_podman(podman_bin)?;
    let name = crate::config::resource_name(&format!("transfer-{}", uuid::Uuid::now_v7().simple()));
    let created = podman_at(podman_bin)
        .args(["create", "--name", &name])
        .args(user_args(user))
        .args([
            "--mount",
            &format!("type=volume,src={volume},dst=/data"),
            image,
            "true",
        ])
        .output()
        .await
        .map_err(BoundaryError::Io)?;
    if !created.status.success() {
        return Err(BoundaryError::Other(format!(
            "create runtime transfer: {}",
            String::from_utf8_lossy(&created.stderr).trim()
        )));
    }
    let copied = copy_out(podman_bin, &format!("{name}:/data/."), destination).await;
    let _ = podman_at(podman_bin)
        .args(["rm", "-f", &name])
        .output()
        .await;
    copied?;
    crate::workspace::validate_tree(destination).map_err(|e| BoundaryError::Other(e.to_string()))
}

/// `podman volume ls --format '{{.Name}} {{.CreatedAt.Unix}}'`, one volume a line.
fn parse_volume_list(out: &str) -> Vec<super::VolumeInfo> {
    out.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let name = fields.next()?.to_string();
            if !crate::gc::is_own(&name) {
                return None;
            }
            let created_ms = fields
                .next()
                .and_then(|secs| secs.parse::<i64>().ok())
                .map(|secs| secs * 1000);
            Some(super::VolumeInfo { name, created_ms })
        })
        .collect()
}

/// `podman ps -a --format json`, the transfers' containers only: a name
/// that only starts like one (a filter is a pattern) is not one.
fn parse_transfer_list(out: &str) -> Result<Vec<super::TransferInfo>, BoundaryError> {
    let rows: Vec<serde_json::Value> = match out.trim() {
        "" => Vec::new(),
        out => serde_json::from_str(out)
            .map_err(|e| BoundaryError::Other(format!("podman ps: {e}")))?,
    };
    let stem = crate::config::resource_name("transfer-");
    Ok(rows
        .iter()
        .filter_map(|row| {
            let name = row["Names"]
                .as_array()?
                .iter()
                .filter_map(|n| n.as_str())
                .find(|n| n.starts_with(&stem))?
                .to_string();
            Some(super::TransferInfo {
                name,
                running: row["State"].as_str() == Some("running"),
                created_ms: row["Created"].as_i64().map(|secs| secs * 1000),
                image: row["ImageID"].as_str().unwrap_or_default().to_string(),
            })
        })
        .collect())
}

pub struct PodmanBackend {
    cfg: Config,
    selinux: bool,
    /// Who may leave through the node's proxy, and to where. Issued here,
    /// honoured by the listener the node serves behind the gateway's forward.
    grants: crate::gateway::proxy::Grants,
}

impl PodmanBackend {
    pub async fn detect(cfg: &Config) -> Self {
        // Before the first spawn: everything after this, `selinux_enabled`
        // included, goes through `podman()`.
        let _ = PODMAN_BIN.set(resolve_podman_env(cfg));
        Self {
            cfg: cfg.clone(),
            selinux: selinux_enabled().await,
            grants: Default::default(),
        }
    }

    pub fn spec(&self) -> RunSpec {
        RunSpec::from_config(&self.cfg, self.selinux)
    }

    /// The node's own store when it keeps `volume`, or when it is to keep a
    /// new one of this name. A volume it does not (a dependency cache, or a
    /// workspace a node made before its trees were its own) is Podman's.
    fn files_for(&self, volume: &str) -> Option<crate::runner::sandbox_files::FilesStore> {
        let store = crate::runner::sandbox_files::FilesStore::for_node();
        let keeps = store.holds(volume) || crate::runner::sandbox_files::backs(volume);
        keeps.then_some(store)
    }

    /// Copy a staging tree into a volume the node keeps. A check's copy of
    /// an immutable snapshot is the snapshot itself, sent straight into its
    /// sandbox; nothing reads a check's changes back.
    fn files_import(
        store: &crate::runner::sandbox_files::FilesStore,
        volume: &str,
        source: &Path,
    ) -> Result<(), BoundaryError> {
        use crate::runner::sandbox_files as files;
        if files::is_live(volume) {
            return Err(BoundaryError::Other(format!(
                "volume {volume} is in use by a running sandbox"
            )));
        }
        let done = match files::is_check(volume) {
            true => crate::workspace::validate_tree(source)
                .map_err(files::FilesError::from)
                .and_then(|()| store.alias(volume, source)),
            false => store.import(volume, source),
        };
        done.map_err(|e| BoundaryError::Other(format!("copy into {volume}: {e}")))
    }

    /// Export a volume the node keeps: from the sandbox holding it while one
    /// runs, else the node's copy.
    async fn files_export(
        store: &crate::runner::sandbox_files::FilesStore,
        volume: &str,
        destination: &Path,
        checkout: bool,
    ) -> Result<(), BoundaryError> {
        let failed = |e: crate::runner::sandbox_files::FilesError| {
            BoundaryError::Other(format!("copy out of {volume}: {e}"))
        };
        match crate::runner::sandbox_files::export_live(volume, destination, checkout).await {
            Some(exported) => exported.map(|_| ()).map_err(failed)?,
            None => {
                let store = store.clone();
                let (volume, destination) = (volume.to_string(), destination.to_path_buf());
                tokio::task::spawn_blocking(move || store.export(&volume, &destination, checkout))
                    .await
                    .map_err(|e| BoundaryError::Other(e.to_string()))?
                    .map_err(failed)?
            }
        }
        crate::workspace::validate_tree(destination)
            .map_err(|e| BoundaryError::Other(e.to_string()))
    }

    /// The runner for `spec`: a sandbox per command, over the protocol.
    fn runner_with(&self, spec: RunSpec) -> Arc<dyn Runner> {
        protocol_runner(&self.cfg, spec)
    }
}

/// A sandbox per command, made by the Podman provider around `spec`. The
/// provider takes the boundary; the runner keeps `spec`'s mounts, image and
/// working directory, sends the trees in and takes them back over the files
/// channel, opens the gateway's tunnels, runs bundled services under the
/// supervisor, and hands image identity and image-only services (sidecars)
/// on to a `PodmanRunner`.
pub fn protocol_runner(cfg: &Config, spec: RunSpec) -> Arc<dyn Runner> {
    let mounts = spec.extra_mounts.clone();
    let image = spec.image.clone();
    let workdir = spec.workdir.clone();
    let mut boundary = spec.clone();
    boundary.extra_mounts = Vec::new();
    let provider = crate::provider::podman::PodmanProvider::from_config(cfg, boundary);
    let runner = crate::runner::protocol::ProtocolRunner::new(Arc::new(provider), image)
        .with_mounts(mounts)
        .with_workdir(workdir)
        .with_delegate(Arc::new(PodmanRunner::new(spec)))
        .with_bundled_harness(true)
        .with_sandbox_services(true)
        .with_files(crate::runner::sandbox_files::FilesStore::for_node())
        .with_tunnels(crate::runner::tunnels::gateway_tunnels(cfg));
    Arc::new(runner)
}

/// The image the node's own short-lived containers (volume transfers and
/// copies) run, and the user they run as: the stock base, as root (the
/// base's own user would be a subordinate one here), which is the node's own
/// user.
fn utility_image(cfg: &Config) -> (String, Option<&'static str>) {
    (
        crate::sandbox_images::base_image(cfg),
        Some(crate::sandbox_images::UTILITY_USER),
    )
}

/// `--user` for a utility container, where its image's own user is not the
/// one.
fn user_args(user: Option<&str>) -> Vec<String> {
    user.map(|user| vec!["--user".to_string(), user.to_string()])
        .unwrap_or_default()
}

#[async_trait]
impl Backend for PodmanBackend {
    fn kind(&self) -> &'static str {
        "podman"
    }

    async fn setup(&self, cfg: &Config, rebuild: bool) -> Result<(), BoundaryError> {
        setup::setup(cfg, rebuild).await
    }

    async fn converge(&self, cfg: &Config) -> Result<bool, BoundaryError> {
        setup::converge(cfg).await
    }

    fn converges(&self) -> bool {
        true
    }

    fn definitions(&self, cfg: &Config) -> Option<String> {
        Some(setup::definitions(cfg))
    }

    async fn check_all(&self, cfg: &Config, deep: bool) -> BoundaryReport {
        checks::check_all(cfg, self.selinux, deep).await
    }

    async fn harness_images(&self, cfg: &Config) -> Vec<crate::boundary::HarnessImage> {
        setup::harness_images(cfg).await
    }

    fn runner(&self, extra_mounts: Vec<Mount>) -> Arc<dyn Runner> {
        self.runner_for(&self.cfg.harness.id, extra_mounts)
    }

    fn runner_for(&self, harness_id: &str, extra_mounts: Vec<Mount>) -> Arc<dyn Runner> {
        let mut spec = RunSpec::for_harness(&self.cfg, harness_id, self.selinux);
        spec.extra_mounts = extra_mounts;
        self.runner_with(spec)
    }

    fn runner_in(
        &self,
        harness_id: &str,
        image: &str,
        extra_mounts: Vec<Mount>,
    ) -> Arc<dyn Runner> {
        let mut spec = RunSpec::for_harness(&self.cfg, harness_id, self.selinux);
        spec.image = image.to_string();
        spec.extra_mounts = extra_mounts;
        self.runner_with(spec)
    }

    fn image_builder(&self) -> Option<&dyn super::ImageBuilder> {
        Some(self)
    }

    async fn prepared_snapshots(&self) -> Vec<String> {
        crate::sandbox_images::snapshot_images()
            .await
            .into_iter()
            .map(|(pinned, _)| pinned)
            .collect()
    }

    fn sandbox_files_store(&self) -> Option<crate::runner::sandbox_files::FilesStore> {
        Some(crate::runner::sandbox_files::FilesStore::for_node())
    }

    fn preparer(&self) -> Option<crate::sandbox_images::Preparer> {
        let cfg = &self.cfg;
        Some(crate::sandbox_images::Preparer {
            provider: Arc::new(crate::provider::podman::PodmanProvider::from_config(
                cfg,
                crate::runner::podman::RunSpec::from_config(cfg, false),
            )),
            // A preparation's tree goes over the files channel into the
            // sandbox's own filesystem, so its commit carries it.
            files: Some(crate::runner::sandbox_files::FilesStore::for_node()),
            tunnels: crate::runner::tunnels::gateway_tunnels(cfg),
        })
    }

    fn runs_sessions_in_repository_images(&self) -> bool {
        true
    }

    async fn import_volume(&self, volume: &str, source: &Path) -> Result<(), BoundaryError> {
        if let Some(store) = self.files_for(volume) {
            return Self::files_import(&store, volume, source);
        }
        let _images = super::reconcile::images_in_use().await;
        let (image, user) = utility_image(&self.cfg);
        volume_copy_in(
            &crate::boundary::podman::resolve_podman_env(&self.cfg),
            (&image, user),
            volume,
            source,
        )
        .await
    }

    async fn import_writable(&self, volume: &str, source: &Path) -> Result<(), BoundaryError> {
        // The node's copy carries only whether a file is executable, so a
        // tree it keeps is writable already.
        if let Some(store) = self.files_for(volume) {
            return Self::files_import(&store, volume, source);
        }
        let _images = super::reconcile::images_in_use().await;
        let (image, user) = utility_image(&self.cfg);
        volume_copy_in(
            &crate::boundary::podman::resolve_podman_env(&self.cfg),
            (&image, user),
            volume,
            source,
        )
        .await?;
        // The owner may always change its own modes, so this needs no
        // capability, and nothing leaves the volume.
        let mount = format!("type=volume,src={volume},dst=/data");
        let mut args = vec!["run", "--rm", "--network", "none", "--cap-drop=ALL"];
        args.extend(["--security-opt=no-new-privileges"]);
        if let Some(user) = user {
            args.extend(["--user", user]);
        }
        args.extend([
            "--entrypoint",
            "chmod",
            "--mount",
            &mount,
            &image,
            "-R",
            "u+w",
            "/data",
        ]);
        podman(&args).await.map(|_| ())
    }

    async fn export_volume(&self, volume: &str, destination: &Path) -> Result<(), BoundaryError> {
        if let Some(store) = self.files_for(volume).filter(|s| s.holds(volume)) {
            return Self::files_export(&store, volume, destination, false).await;
        }
        let _images = super::reconcile::images_in_use().await;
        let (image, user) = utility_image(&self.cfg);
        volume_copy_out(
            &crate::boundary::podman::resolve_podman_env(&self.cfg),
            (&image, user),
            volume,
            destination,
        )
        .await
    }

    async fn export_workspace(
        &self,
        volume: &str,
        destination: &Path,
    ) -> Result<(), BoundaryError> {
        if let Some(store) = self.files_for(volume).filter(|s| s.holds(volume)) {
            return Self::files_export(&store, volume, destination, true).await;
        }
        let _images = super::reconcile::images_in_use().await;
        let (image, user) = utility_image(&self.cfg);
        workspace_copy_out(
            &crate::boundary::podman::resolve_podman_env(&self.cfg),
            (&image, user),
            volume,
            destination,
        )
        .await
    }

    fn harness_host(&self) -> String {
        self.cfg.boundary.gateway_container.clone()
    }

    fn harness_home(&self) -> String {
        crate::sandbox_images::HOME.into()
    }

    async fn reconcile(&self, names: &[String]) {
        use crate::provider::SandboxProvider as _;
        let provider = crate::provider::podman::PodmanProvider::from_config(&self.cfg, self.spec());
        for name in names {
            let _ = podman(&["rm", "-f", "-i", name]).await;
            // And the socket directory a sandbox of that name had.
            let _ = provider
                .destroy(&crate::provider::Handle(name.clone()))
                .await;
        }
    }

    async fn list_volumes(&self) -> Result<Vec<super::VolumeInfo>, BoundaryError> {
        let filter = format!("name=^{}-", crate::config::resource_prefix());
        let out = podman(&[
            "volume",
            "ls",
            "--filter",
            &filter,
            "--format",
            "{{.Name}} {{.CreatedAt.Unix}}",
        ])
        .await?;
        let mut volumes = parse_volume_list(&out);
        let kept = crate::runner::sandbox_files::FilesStore::for_node()
            .list()
            .map_err(|e| BoundaryError::Other(e.to_string()))?;
        volumes.extend(kept);
        volumes.sort_by(|a, b| a.name.cmp(&b.name));
        volumes.dedup_by(|a, b| a.name == b.name);
        Ok(volumes)
    }

    async fn remove_volume(&self, volume: &str) -> Result<(), BoundaryError> {
        let store = crate::runner::sandbox_files::FilesStore::for_node();
        if store.holds(volume) {
            // As Podman refuses to remove a volume a container mounts.
            if crate::runner::sandbox_files::is_live(volume) {
                return Err(BoundaryError::Other(format!(
                    "volume {volume} is in use by a running sandbox"
                )));
            }
            return store
                .remove(volume)
                .map_err(|e| BoundaryError::Other(e.to_string()));
        }
        // Never `--force`: a volume a harness still mounts is refused.
        match podman(&["volume", "rm", volume]).await {
            Ok(_) => Ok(()),
            Err(BoundaryError::Podman(stderr)) if stderr.contains("no such volume") => Ok(()),
            Err(error) => Err(error),
        }
    }

    async fn list_transfers(&self) -> Result<Vec<super::TransferInfo>, BoundaryError> {
        let filter = format!("name=^{}", crate::config::resource_name("transfer-"));
        let out = podman(&["ps", "-a", "--filter", &filter, "--format", "json"]).await?;
        parse_transfer_list(&out)
    }

    /// Never `--force`: a copy still running is refused.
    async fn remove_transfer(&self, name: &str) -> Result<(), BoundaryError> {
        match podman(&["rm", name]).await {
            Ok(_) => Ok(()),
            Err(BoundaryError::Podman(stderr)) if stderr.contains("no such container") => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// A workspace from a snapshot is sent into the snapshot's own `/work`
    /// (`FilesStore::plan_from`).
    fn workspace_over_image(&self) -> bool {
        true
    }

    async fn volume_exists(&self, volume: &str) -> bool {
        crate::runner::sandbox_files::FilesStore::for_node().holds(volume)
            || podman(&["volume", "exists", volume]).await.is_ok()
    }

    /// A reflink where the storage has them, so a prepared tree or a warmed
    /// cache costs a clone almost nothing; an ordinary copy where it does not.
    /// The copying container has no network and no capabilities, and reads
    /// its source read-only. A source that does not exist is created empty by
    /// the mount, which is what an empty destination is a copy of.
    async fn clone_volume(&self, source: &str, destination: &str) -> Result<(), BoundaryError> {
        // A tree the node keeps is copied (or, for a check, named) here; the
        // check's sandbox is sent it straight from the node's copy.
        if let Some(store) = self.files_for(source).filter(|s| s.holds(source)) {
            return store
                .clone_volume(source, destination)
                .await
                .map_err(|e| BoundaryError::Other(format!("copy {source}: {e}")));
        }
        let _images = super::reconcile::images_in_use().await;
        let (image, user) = utility_image(&self.cfg);
        // A copy of a base cache whose repository has a prepared snapshot is
        // filled from the snapshot's cache instead: Podman copies what the
        // image has at a mount point into a new, empty volume mounted there.
        // In the sandboxes' own user namespace, so what it copies is the
        // node's, as the sandboxes see it.
        if let Some(snapshot) = crate::sandbox_images::seed_for(source) {
            podman(&["volume", "create", "--ignore", destination]).await?;
            let mount = format!("type=volume,src={destination},dst=/cache");
            let userns = format!("--userns={}", crate::sandbox_images::USERNS);
            return podman(&[
                "run",
                "--rm",
                "--network",
                "none",
                "--cap-drop=ALL",
                "--security-opt=no-new-privileges",
                &userns,
                "--user",
                crate::sandbox_images::USER,
                "--entrypoint",
                "true",
                "--mount",
                &mount,
                &snapshot,
            ])
            .await
            .map(|_| ());
        }
        let from = format!("type=volume,src={source},dst=/from,ro");
        let to = format!("type=volume,src={destination},dst=/to");
        let mut args = vec!["run", "--rm", "--network", "none", "--cap-drop=ALL"];
        args.extend(["--security-opt=no-new-privileges"]);
        if let Some(user) = user {
            args.extend(["--user", user]);
        }
        args.extend([
            "--entrypoint",
            "sh",
            "--mount",
            &from,
            "--mount",
            &to,
            &image,
            "-c",
            CLONE_TREE,
        ]);
        podman(&args).await.map(|_| ())
    }

    fn egress_grants(&self) -> Option<&crate::gateway::proxy::Grants> {
        Some(&self.grants)
    }

    fn egress_grant(
        &self,
        spec: crate::gateway::proxy::GrantSpec,
    ) -> Result<super::EgressGrant, BoundaryError> {
        if self.cfg.gateway.qa_proxy_port == 0 {
            return Err(BoundaryError::Podman(
                "per-client egress is not configured (gateway.qa_proxy_port)".into(),
            ));
        }
        let username = spec.client.clone();
        let token = self
            .grants
            .issue(spec)
            .map_err(|error| BoundaryError::Other(format!("egress grant: {error}")))?;
        let grants = self.grants.clone();
        let revoked = token.clone();
        Ok(super::EgressGrant::new(
            format!(
                "http://{}:{}",
                self.cfg.boundary.gateway_container, self.cfg.gateway.qa_proxy_port
            ),
            username,
            token,
            move || grants.revoke(&revoked),
        ))
    }
}

static PODMAN_BIN: OnceLock<String> = OnceLock::new();

/// The resolved podman binary. Before any backend was detected (unit tests,
/// mostly) this is the bare name, which resolves through PATH as it always
/// did.
pub(crate) fn podman_bin() -> &'static str {
    PODMAN_BIN.get().map(String::as_str).unwrap_or("podman")
}

/// Resolve which podman to run. The node often lives under a launcher whose
/// PATH is not a login shell's — Finder hands out launchd's minimal PATH,
/// which has no Homebrew — so a bare `Command::new("podman")` fails with
/// ENOENT while the terminal works fine. Order: the operator's explicit
/// config, verbatim; a PATH hit; well-known install locations; the bare name,
/// so the spawn error still says what was tried.
pub fn resolve_podman(
    explicit: &str,
    path_var: Option<&std::ffi::OsStr>,
    candidates: &[&Path],
) -> String {
    if !explicit.is_empty() {
        return explicit.to_string();
    }
    if let Some(path) = path_var {
        for dir in std::env::split_paths(path) {
            let p = dir.join("podman");
            if p.is_file() {
                return p.to_string_lossy().into_owned();
            }
        }
    }
    for c in candidates {
        if c.is_file() {
            return c.to_string_lossy().into_owned();
        }
    }
    "podman".into()
}

/// Where podman is usually installed, for a node whose PATH lacks it.
pub(crate) fn podman_candidates() -> [PathBuf; 3] {
    [
        PathBuf::from("/opt/homebrew/bin/podman"),
        PathBuf::from("/usr/local/bin/podman"),
        PathBuf::from("/usr/bin/podman"),
    ]
}

/// The podman this node runs, as `resolve_podman` finds it but through the
/// path guard: a PATH entry or a configured path an agent could have written
/// is passed over or refused. A refused one is returned as configured, so
/// every spawn's own check refuses it again with the reason.
pub(crate) fn resolve_podman_env(cfg: &Config) -> String {
    let candidates = podman_candidates();
    let refs: Vec<&Path> = candidates.iter().map(PathBuf::as_path).collect();
    match crate::config::path_guard::resolve("boundary.podman", &cfg.boundary.podman, &refs) {
        Some(Ok(path)) => path.to_string_lossy().into_owned(),
        Some(Err(refusal)) => {
            tracing::warn!(%refusal, "podman");
            match cfg.boundary.podman.is_empty() {
                true => "podman".into(),
                false => cfg.boundary.podman.clone(),
            }
        }
        None => resolve_podman(
            &cfg.boundary.podman,
            std::env::var_os("PATH").as_deref(),
            &refs,
        ),
    }
}

/// A podman command for a path `guarded_podman` returned, given the PATH
/// the guard leaves a child.
fn podman_at(bin: &str) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(bin);
    command.env("PATH", crate::config::path_guard::child_path());
    command
}

/// `bin` as the path guard finds it now, for the spawns of one operation.
pub(super) fn guarded_podman(bin: &str) -> std::io::Result<String> {
    crate::config::path_guard::program("boundary.podman", bin)
        .map(|path| path.to_string_lossy().into_owned())
}

/// The `podman build` arguments that hold a build's `RUN` steps to the
/// node's egress proxy on loopback `port`: a network namespace whose only
/// route is that one port spliced to the node's loopback (no interface, no
/// DNS), and the proxy variables taken from podman's own environment, so the
/// grant's credentials are on no argv. The host's own proxy settings are not
/// passed through.
pub(crate) fn build_egress_args(port: u16) -> Vec<String> {
    let mut args = vec![
        "--network".to_string(),
        format!("pasta:--splice-only,-T,{port}"),
        "--http-proxy=false".to_string(),
    ];
    for name in super::BUILD_PROXY_VARS {
        args.extend(["--build-arg".to_string(), name.to_string()]);
    }
    args
}

impl PodmanBackend {
    /// `podman build`, with its steps held to the proxy at loopback `port`
    /// through `proxy` (the grant's URL) when `egress` is given.
    async fn build_in(
        &self,
        build: super::ImageBuild<'_>,
        egress: Option<(u16, &str)>,
    ) -> Result<super::BuiltImage, super::BuildFailure> {
        let bin = match guarded_podman(&resolve_podman_env(&self.cfg)) {
            Ok(bin) => bin,
            Err(error) => {
                return Err(super::BuildFailure {
                    error: format!("podman build: {error}"),
                    ..Default::default()
                })
            }
        };
        let mut command = podman_at(&bin);
        command.arg("build");
        for (name, value) in build.labels {
            command.arg("--label").arg(format!("{name}={value}"));
        }
        if let Some((port, proxy)) = egress {
            command.args(build_egress_args(port));
            for name in super::BUILD_PROXY_VARS {
                command.env(name, proxy);
            }
        }
        command
            .arg("-f")
            .arg(build.containerfile)
            .arg("-t")
            .arg(build.tag)
            .arg(build.context)
            .stdin(std::process::Stdio::null())
            // A build abandoned at its deadline must not keep running.
            .kill_on_drop(true);
        let failure = |error: String, log: &[u8]| super::BuildFailure {
            error,
            log_tail: log_tail(log),
            ..Default::default()
        };
        let out = match tokio::time::timeout(build.timeout, command.output()).await {
            Ok(Ok(out)) => out,
            Ok(Err(error)) => return Err(failure(format!("podman build: {error}"), &[])),
            Err(_) => {
                return Err(failure(
                    format!(
                        "the build did not finish in {} minutes",
                        build.timeout.as_secs() / 60
                    ),
                    &[],
                ))
            }
        };
        let log = [out.stdout, out.stderr].concat();
        if !out.status.success() {
            let exit = out.status.code().unwrap_or(-1);
            return Err(failure(format!("podman build exited {exit}"), &log));
        }
        let Some(image) = super::ImageBuilder::identity(self, build.tag).await else {
            return Err(failure(
                format!("the runtime reported no digest for {}", build.tag),
                &log,
            ));
        };
        Ok(super::BuiltImage {
            image,
            log_tail: log_tail(&log),
        })
    }
}

#[async_trait]
impl super::ImageBuilder for PodmanBackend {
    async fn build(
        &self,
        build: super::ImageBuild<'_>,
    ) -> Result<super::BuiltImage, super::BuildFailure> {
        self.build_in(build, None).await
    }

    /// The steps reach the node's egress proxy through a listener of this
    /// build's own on the node's loopback, under a grant of exactly `hosts`:
    /// a route that needs no gateway container, so it is the same with
    /// `sandbox_tunnels` as without. Both go when the build ends.
    async fn build_with_egress(
        &self,
        build: super::ImageBuild<'_>,
        hosts: &[String],
    ) -> Result<super::BuiltImage, super::BuildFailure> {
        // Gone with the build, however it ends, abandoned at its deadline
        // included.
        let serving =
            super::BuildEgress::serve(&self.grants, super::Backend::egress_refusal(self), hosts)
                .await?;
        let (port, proxy) = (serving.port, serving.proxy(serving.port));
        self.build_in(build, Some((port, &proxy))).await
    }

    async fn exists(&self, image: &str) -> bool {
        podman(&["image", "exists", image]).await.is_ok()
    }

    async fn identity(&self, image: &str) -> Option<String> {
        image_identity(image).await
    }

    /// A pinned `name@sha256:…` goes with every tag of that name on the
    /// image. Each build has a tag of its own, so a cached rebuild that made
    /// the same image again left it under two, and Podman refuses to remove
    /// an image with more than one name by its digest. All but the last are
    /// taken off first; the last goes with the image, or stays with it while
    /// a container uses it, so the digest still resolves for that one.
    async fn remove(&self, image: &str) {
        if let Some((name, _)) = image.split_once('@') {
            let format = "{{.Id}}{{range .RepoTags}} {{.}}{{end}}";
            if let Ok(out) = podman(&["image", "inspect", "--format", format, image]).await {
                let mut fields = out.split_whitespace();
                let id = fields.next().unwrap_or_default().to_string();
                let tags: Vec<&str> = fields
                    .filter(|tag| crate::image_gc::repository_of(tag) == name)
                    .collect();
                if let Some((_, rest)) = tags.split_last() {
                    for tag in rest {
                        let _ = podman(&["untag", &id, tag]).await;
                    }
                }
            }
        }
        let _ = podman(&["image", "rm", image]).await;
    }
}

/// Whether `image` can only be in this host's store: Podman's name for what
/// it built or committed. One that is missing is not anywhere, yet Podman
/// asks `https://localhost/v2/` for it unless told not to pull.
pub(crate) fn local_only(image: &str) -> bool {
    image.starts_with("localhost/")
}

/// `image` as `name@sha256:…`, the reference a later tag cannot move, if
/// the store has it.
pub(crate) async fn image_identity(image: &str) -> Option<String> {
    let digest = podman(&["image", "inspect", "--format", "{{.Digest}}", image])
        .await
        .ok()?;
    // A name already pinned keeps its name; a tag is the last `:` after the
    // last `/`; a registry's port is not.
    let name = match image.split_once('@') {
        Some((name, _)) => name,
        None => match image.rsplit_once(':') {
            Some((name, tag)) if !tag.contains('/') => name,
            _ => image,
        },
    };
    let identity = format!("{name}@{}", digest.trim());
    crate::config::immutable_image(&identity)
        .is_ok()
        .then_some(identity)
}

pub(crate) fn log_tail(bytes: &[u8]) -> String {
    const MAX: usize = 8192;
    let text = String::from_utf8_lossy(bytes);
    let start = text.len().saturating_sub(MAX);
    let start = (start..text.len())
        .find(|at| text.is_char_boundary(*at))
        .unwrap_or(text.len());
    text[start..].to_string()
}

/// Run `podman` with args and return stdout, or the stderr as an error.
pub(crate) async fn podman(args: &[&str]) -> Result<String, BoundaryError> {
    let bin = podman_bin();
    let unusable = |e: std::io::Error| {
        if e.kind() == std::io::ErrorKind::NotFound {
            BoundaryError::Other(format!(
                "podman not found (tried `{bin}`); install podman or set `podman = \"/path/to/podman\"` under [boundary] in node.toml"
            ))
        } else {
            BoundaryError::Io(e)
        }
    };
    let out = podman_at(&guarded_podman(bin).map_err(unusable)?)
        .args(args)
        .output()
        .await
        .map_err(unusable)?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(BoundaryError::Podman(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ))
    }
}

pub(crate) async fn podman_json(args: &[&str]) -> Result<serde_json::Value, BoundaryError> {
    let text = podman(args).await?;
    Ok(serde_json::from_str(&text)?)
}

/// Whether the container host enforces SELinux (Podman needs `label=disable`
/// for bind mounts when it does).
pub async fn selinux_enabled() -> bool {
    podman(&["info", "--format", "{{.Host.Security.SELinuxEnabled}}"])
        .await
        .map(|s| s.trim() == "true")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::resolve_podman;
    use std::path::{Path, PathBuf};

    /// A build's steps get one port of the node's loopback and nothing
    /// else, and the proxy variables by name only: their values, which carry
    /// the grant's credentials, come from podman's environment.
    #[test]
    fn build_steps_reach_only_the_proxy_port_and_never_name_its_credentials() {
        let args = super::build_egress_args(41234);
        assert_eq!(
            args[..3],
            [
                "--network",
                "pasta:--splice-only,-T,41234",
                "--http-proxy=false"
            ]
        );
        let named: Vec<&str> = args[3..]
            .chunks(2)
            .map(|pair| {
                assert_eq!(pair[0], "--build-arg");
                pair[1].as_str()
            })
            .collect();
        assert_eq!(named, crate::boundary::BUILD_PROXY_VARS);
        assert!(args.iter().all(|arg| !arg.contains('@')), "{args:?}");
    }

    /// Every sandbox is made by the provider from the bundle; the settings
    /// it reads are its own block's and the sandbox section's.
    #[test]
    fn sandboxes_read_their_settings_and_sockets_from_the_node() {
        let default = crate::config::Config::default();
        assert!(!default.runtime.podman.sandbox_init);
        assert!(default.sandbox.bundle.is_none());
        let on: crate::config::Config = toml::from_str(
            "[runtime.podman]\nsandbox_init = true\n[sandbox]\nbundle = \"/opt/bundle\"\n",
        )
        .unwrap();
        assert!(on.runtime.podman.sandbox_init);
        assert_eq!(on.sandbox.bundle.as_deref(), Some(Path::new("/opt/bundle")));
        // Beside the harness socket, so a node with its own keeps its own.
        let mut scratch = on.clone();
        scratch.gateway.harness_listen =
            crate::config::HarnessListen::Unix("/tmp/n1/harness.sock".into());
        assert_eq!(scratch.sandbox_socket_dir(), Path::new("/tmp/n1/sandbox"));
        assert_eq!(
            scratch.allow_proxy_socket(),
            Path::new("/tmp/n1/allow.sock")
        );
    }

    /// A sandbox's mounts are named volumes the provider mounts. Storage of
    /// the sandbox's own is an anonymous volume owned by the image's user,
    /// which `--rm` removes with it.
    #[test]
    fn only_a_planned_mount_is_storage_of_the_sandboxs_own() {
        use crate::runner::podman::{SandboxEntry, SandboxLaunch};
        use crate::runner::Mount;
        let launch = SandboxLaunch {
            bundle: "/b".into(),
            socket_dir: "/s".into(),
            pin: "sha256:00".into(),
            entry: SandboxEntry::Pid1,
            cpu_millis: None,
            memory_bytes: None,
            connect_timeout_ms: 1000,
            log_file: None,
            user: None,
            userns: None,
            env: Vec::new(),
            service_bundles: Vec::new(),
        };
        let spec = super::RunSpec::from_config(&crate::config::Config::default(), false);
        let mounts = vec![
            Mount::volume(
                crate::session::materialize::harness_state_volume(),
                "/root/.claude",
                false,
            ),
            Mount::volume("tracon-workspace-x", "/work", false),
        ];
        let joined = spec
            .sandbox_args("tracon-h-x", "img", &mounts, &launch, false)
            .join(" ");
        assert!(joined.contains("type=volume,src=tracon-harness-state,dst=/root/.claude"));
        assert!(joined.contains("type=volume,src=tracon-workspace-x,dst=/work"));
        assert!(!joined.contains("U=true"));
        let local = spec
            .sandbox_args(
                "tracon-h-x",
                "img",
                &[Mount::local("/work")],
                &launch,
                false,
            )
            .join(" ");
        assert!(local.contains("--mount type=volume,dst=/work,U=true"));
    }

    #[test]
    fn a_bundled_service_is_mounted_and_an_image_one_stays_a_sidecar() {
        let catalogue = "[[service]]\nname = \"browser\"\nport = 9222\n\
                         bundle = \"/srv/browser/amd64\"\n\
                         [[service]]\nname = \"db\"\nport = 5432\n\
                         image = \"pg@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"\n";
        let on: crate::config::Config = toml::from_str(catalogue).unwrap();
        assert_eq!(
            crate::provider::podman::service_bundles(&on),
            [(
                PathBuf::from("/srv/browser/amd64"),
                "/opt/tracon-services/browser".to_string()
            )]
        );
    }

    fn touch(dir: &Path, name: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, "").unwrap();
        p
    }

    #[test]
    fn an_explicit_config_value_wins_even_when_it_does_not_exist() {
        // The operator said so; a wrong path should fail loudly at spawn, not
        // be silently second-guessed.
        let got = resolve_podman("/nonexistent/podman", None, &[]);
        assert_eq!(got, "/nonexistent/podman");
    }

    #[test]
    fn a_path_hit_beats_the_candidates() {
        let dir = std::env::temp_dir().join(format!("tracon-podman-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("on-path")).unwrap();
        std::fs::create_dir_all(dir.join("candidate")).unwrap();
        let on_path = touch(&dir.join("on-path"), "podman");
        let candidate = touch(&dir.join("candidate"), "podman");
        let path_var = std::env::join_paths([dir.join("on-path")]).unwrap();
        let got = resolve_podman("", Some(&path_var), &[&candidate]);
        assert_eq!(got, on_path.to_string_lossy());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn candidates_rescue_a_minimal_path_and_the_bare_name_is_last() {
        let dir = std::env::temp_dir().join(format!("tracon-podman-cand-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let candidate = touch(&dir, "podman");
        let empty = std::ffi::OsString::new();
        let got = resolve_podman("", Some(&empty), &[&candidate]);
        assert_eq!(got, candidate.to_string_lossy());
        // Nothing anywhere: the bare name, so the spawn error names it.
        assert_eq!(resolve_podman("", Some(&empty), &[]), "podman");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The clone's flags still copy everything a cache is: modes, mtimes,
    /// symlinks as symlinks, and hard links as links. Run with the host's GNU
    /// `cp`, which is what the shipped harness images carry; macOS's BSD `cp`
    /// has neither `-d` nor `--reflink` and never runs this command.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_clone_copies_modes_times_and_links() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        use std::time::{Duration, SystemTime};

        let dir = tempfile::tempdir().unwrap();
        let (from, to) = (dir.path().join("from"), dir.path().join("to"));
        std::fs::create_dir_all(from.join("sub")).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        let file = from.join("sub/file");
        std::fs::write(&file, "cached").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1_577_934_245);
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        std::fs::hard_link(&file, from.join("hard")).unwrap();
        std::os::unix::fs::symlink("sub/file", from.join("link")).unwrap();

        let script = super::CLONE_TREE
            .replace("/from/", &format!("{}/", from.display()))
            .replace("/to/", &format!("{}/", to.display()));
        let out = std::process::Command::new("sh")
            .args(["-c", &script])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );

        let copied = std::fs::metadata(to.join("sub/file")).unwrap();
        assert_eq!(
            std::fs::read_to_string(to.join("sub/file")).unwrap(),
            "cached"
        );
        assert_eq!(copied.permissions().mode() & 0o777, 0o640);
        assert_eq!(copied.modified().unwrap(), mtime);
        assert_eq!(copied.nlink(), 2);
        assert_eq!(
            std::fs::metadata(to.join("hard")).unwrap().ino(),
            copied.ino()
        );
        assert_eq!(
            std::fs::read_link(to.join("link")).unwrap(),
            Path::new("sub/file")
        );
    }

    #[test]
    fn the_volume_list_keeps_only_tracon_volumes_with_their_age() {
        let out =
            "tracon-probe-scratch 1789178113\nminikube-config 1789000000\ntracon-workspace-abc\n\n";
        assert_eq!(
            super::parse_volume_list(out),
            vec![
                crate::boundary::VolumeInfo {
                    name: "tracon-probe-scratch".into(),
                    created_ms: Some(1_789_178_113_000),
                },
                crate::boundary::VolumeInfo {
                    name: "tracon-workspace-abc".into(),
                    created_ms: None,
                },
            ]
        );
    }

    #[test]
    fn the_transfer_list_keeps_only_transfers_with_their_state_and_image() {
        let out = r#"[
          {"Names": ["tracon-transfer-01a1"], "State": "exited", "Created": 1789178113, "ImageID": "abc"},
          {"Names": ["tracon-transfer-01a2"], "State": "running", "Created": 1789178114, "ImageID": "def"},
          {"Names": ["tracon-transferish"], "State": "exited", "Created": 1789178115, "ImageID": "ghi"}
        ]"#;
        assert_eq!(
            super::parse_transfer_list(out).unwrap(),
            vec![
                crate::boundary::TransferInfo {
                    name: "tracon-transfer-01a1".into(),
                    running: false,
                    created_ms: Some(1_789_178_113_000),
                    image: "abc".into(),
                },
                crate::boundary::TransferInfo {
                    name: "tracon-transfer-01a2".into(),
                    running: true,
                    created_ms: Some(1_789_178_114_000),
                    image: "def".into(),
                },
            ]
        );
        assert!(super::parse_transfer_list("").unwrap().is_empty());
    }

    /// Against a real Podman and the image named by
    /// `TRACON_E2E_HARNESS_IMAGE`, which should be one whose `/` root may not
    /// write once capabilities are dropped (a Fedora base): a workspace goes
    /// into a volume and comes back out as Git sees it.
    #[tokio::test]
    #[ignore = "needs Podman and a harness image"]
    async fn a_workspace_exports_from_an_image_with_a_read_only_root() {
        let Ok(image) = std::env::var("TRACON_E2E_HARNESS_IMAGE") else {
            eprintln!("skipped: no image named");
            return;
        };
        let source = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(source.path())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q"]);
        std::fs::write(source.path().join(".gitignore"), "target/\n").unwrap();
        std::fs::write(source.path().join("tracked"), "kept").unwrap();
        std::fs::create_dir(source.path().join("target")).unwrap();
        std::fs::write(source.path().join("target/built"), "left behind").unwrap();
        git(&["add", ".gitignore", "tracked"]);

        let podman = "podman";
        let volume = format!("tracon-e2e-export-{}", uuid::Uuid::now_v7().simple());
        let exported = tempfile::tempdir().unwrap();
        let destination = exported.path().join("workspace");
        let result = async {
            super::volume_copy_in(podman, (&image, None), &volume, source.path()).await?;
            super::workspace_copy_out(podman, (&image, None), &volume, &destination).await
        }
        .await;
        let _ = super::podman_at(&super::guarded_podman(podman).unwrap())
            .args(["volume", "rm", "-f", &volume])
            .output()
            .await;
        result.unwrap();

        assert_eq!(
            std::fs::read_to_string(destination.join("tracked")).unwrap(),
            "kept"
        );
        assert!(destination.join(".git").is_dir());
        assert!(!destination.join("target").exists());
    }
}
