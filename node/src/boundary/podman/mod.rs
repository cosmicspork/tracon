//! The rootless Podman boundary: an internal network, a gateway container
//! carrying the allowlist proxy and the node forward, and a harness container
//! on the internal network only. Phase 0 proved it by hand; this is that
//! proof as code.

pub mod checks;
pub mod setup;

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;

use super::{Backend, BoundaryError, BoundaryReport};
use crate::config::Config;
use crate::runner::podman::{PodmanRunner, RunSpec};
use crate::runner::{Mount, Runner};

async fn volume_copy_in(
    podman_bin: &str,
    image: &str,
    volume: &str,
    source: &Path,
) -> Result<(), BoundaryError> {
    crate::workspace::validate_tree(source).map_err(|e| BoundaryError::Other(e.to_string()))?;
    let created = tokio::process::Command::new(podman_bin)
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
    let name = format!("tracon-transfer-{}", uuid::Uuid::now_v7().simple());
    let created = tokio::process::Command::new(podman_bin)
        .args([
            "create",
            "--name",
            &name,
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
    let copied = tokio::process::Command::new(podman_bin)
        .args([
            "cp",
            &format!("{}/.", source.display()),
            &format!("{name}:/data"),
        ])
        .output()
        .await
        .map_err(BoundaryError::Io);
    let _ = tokio::process::Command::new(podman_bin)
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

/// Stages a workspace as Git sees it under `/export`, in the transfer
/// container's own filesystem. The index names the tracked files and the
/// ignore rules decide the untracked ones, so a deleted file stays deleted and
/// build output stays behind. The list and the archive are files rather than a
/// pipe: a failure on either side of a pipe would stage a partial tree as if
/// it were the whole one. A volume with no repository is copied as it is.
const STAGE_WORKSPACE: &str = r#"set -eu
mkdir -p /export
cd /data
if [ -d .git ]; then
  export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
  git -c safe.directory=/data -c core.fsmonitor=false -c core.hooksPath=/dev/null \
    ls-files -z -co --exclude-standard > /tmp/tracon-export.list
  tar --null --ignore-failed-read -T /tmp/tracon-export.list -cf /tmp/tracon-export.tar
  tar -xf /tmp/tracon-export.tar -C /export
  rm -f /tmp/tracon-export.tar /tmp/tracon-export.list
  cp -a .git /export/.git
else
  cp -a /data/. /export/
fi
"#;

/// Export a workspace volume without what its repository ignores. The
/// repository's own configuration is the agent's to write, so Git reads it
/// only here: in a container with no network, no capabilities and the volume
/// read-only, whose output is validated like any other export.
async fn workspace_copy_out(
    podman_bin: &str,
    image: &str,
    volume: &str,
    destination: &Path,
) -> Result<(), BoundaryError> {
    let name = format!("tracon-transfer-{}", uuid::Uuid::now_v7().simple());
    let staged = tokio::process::Command::new(podman_bin)
        .args([
            "run",
            "--name",
            &name,
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
            copy_out(podman_bin, &format!("{name}:/export/."), destination).await
        }
        Ok(staged) => Err(BoundaryError::Other(format!(
            "stage workspace export: {}",
            String::from_utf8_lossy(&staged.stderr).trim()
        ))),
        Err(error) => Err(error),
    };
    let _ = tokio::process::Command::new(podman_bin)
        .args(["rm", "-f", &name])
        .output()
        .await;
    result?;
    crate::workspace::validate_tree(destination).map_err(|e| BoundaryError::Other(e.to_string()))
}

async fn copy_out(podman_bin: &str, source: &str, destination: &Path) -> Result<(), BoundaryError> {
    if destination.exists() {
        std::fs::remove_dir_all(destination)?;
    }
    // `podman cp <container>:<dir>/. <dest>` copies into `<dest>`'s parent when
    // `<dest>` does not exist yet; it must exist first to receive the tree.
    std::fs::create_dir_all(destination)?;
    let copied = tokio::process::Command::new(podman_bin)
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
    image: &str,
    volume: &str,
    destination: &Path,
) -> Result<(), BoundaryError> {
    let name = format!("tracon-transfer-{}", uuid::Uuid::now_v7().simple());
    let created = tokio::process::Command::new(podman_bin)
        .args([
            "create",
            "--name",
            &name,
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
    let _ = tokio::process::Command::new(podman_bin)
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
            if !name.starts_with("tracon-") {
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

pub struct PodmanBackend {
    cfg: Config,
    selinux: bool,
}

impl PodmanBackend {
    pub async fn detect(cfg: &Config) -> Self {
        // Before the first spawn: everything after this, `selinux_enabled`
        // included, goes through `podman()`.
        let _ = PODMAN_BIN.set(resolve_podman_env(cfg));
        Self {
            cfg: cfg.clone(),
            selinux: selinux_enabled().await,
        }
    }

    pub fn spec(&self) -> RunSpec {
        RunSpec::from_config(&self.cfg, self.selinux)
    }
}

#[async_trait]
impl Backend for PodmanBackend {
    fn kind(&self) -> &'static str {
        "podman"
    }

    async fn setup(&self, cfg: &Config, rebuild: bool) -> Result<(), BoundaryError> {
        setup::setup(cfg, rebuild).await
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
        Arc::new(PodmanRunner::new(spec))
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
        Arc::new(PodmanRunner::new(spec))
    }

    fn image_builder(&self) -> Option<&dyn super::ImageBuilder> {
        Some(self)
    }

    async fn import_volume(&self, volume: &str, source: &Path) -> Result<(), BoundaryError> {
        volume_copy_in(
            &crate::boundary::podman::resolve_podman_env(&self.cfg),
            &self.cfg.boundary.harness_image,
            volume,
            source,
        )
        .await
    }

    async fn export_volume(&self, volume: &str, destination: &Path) -> Result<(), BoundaryError> {
        volume_copy_out(
            &crate::boundary::podman::resolve_podman_env(&self.cfg),
            &self.cfg.boundary.harness_image,
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
        workspace_copy_out(
            &crate::boundary::podman::resolve_podman_env(&self.cfg),
            &self.cfg.boundary.harness_image,
            volume,
            destination,
        )
        .await
    }

    fn harness_host(&self) -> String {
        self.cfg.boundary.gateway_container.clone()
    }

    fn harness_home(&self) -> String {
        crate::session::materialize::PODMAN_HARNESS_HOME.into()
    }

    async fn reconcile(&self, names: &[String]) {
        for name in names {
            let _ = podman(&["rm", "-f", "-i", name]).await;
        }
    }

    async fn list_volumes(&self) -> Result<Vec<super::VolumeInfo>, BoundaryError> {
        let out = podman(&[
            "volume",
            "ls",
            "--filter",
            "name=^tracon-",
            "--format",
            "{{.Name}} {{.CreatedAt.Unix}}",
        ])
        .await?;
        Ok(parse_volume_list(&out))
    }

    async fn remove_volume(&self, volume: &str) -> Result<(), BoundaryError> {
        // Never `--force`: a volume a harness still mounts is refused.
        match podman(&["volume", "rm", volume]).await {
            Ok(_) => Ok(()),
            Err(BoundaryError::Podman(stderr)) if stderr.contains("no such volume") => Ok(()),
            Err(error) => Err(error),
        }
    }

    async fn scope_qa_egress(
        &self,
        allowed_hosts: &[String],
    ) -> Result<super::QaEgressGuard, BoundaryError> {
        if self.cfg.gateway.qa_proxy_port == 0 {
            return Err(BoundaryError::Podman(
                "qa browser egress gateway is not configured (gateway.qa_proxy_port)".into(),
            ));
        }
        let permit = QA_EGRESS.lock().await;
        let gateway = self.cfg.boundary.gateway_container.clone();
        setup::write_qa_allowlist(allowed_hosts)?;
        if let Err(error) = reload_scoped_filter(&gateway).await {
            // Nothing was opened, and nothing is left written for a later
            // reload to open by accident.
            let _ = setup::write_qa_allowlist(&[]);
            return Err(error);
        }
        Ok(super::QaEgressGuard::new(permit, move || {
            if let Err(error) = setup::write_qa_allowlist(&[]) {
                tracing::error!(%error, "could not reset scoped egress allowlist");
            }
            if let Err(error) = reload_scoped_filter_blocking(&gateway) {
                tracing::error!(%error, "could not close the scoped egress gateway");
            }
        }))
    }

    fn qa_proxy_url(&self) -> Option<String> {
        Some(format!(
            "http://{}:{}",
            self.cfg.boundary.gateway_container, self.cfg.gateway.qa_proxy_port
        ))
    }
}

/// Serializes QA browser egress scoping: two runs writing the QA allow file
/// at once would let one leak into the other's scope. Ordinary harness
/// sessions never touch this — their egress is the separate, static
/// `allow_hosts` filter, unaffected by QA runs.
static QA_EGRESS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// tinyproxy reads its filter file once and again only on `SIGUSR1`, so a
/// rewritten allow file changes nothing until the scoped proxy is told. It is
/// the gateway's second tinyproxy, not its PID 1, so the signal goes by its
/// pid file rather than through `podman kill`.
const RELOAD_SCOPED_FILTER: &str = r#"kill -USR1 "$(cat /run/tinyproxy-qa.pid)""#;

/// How long the scoped proxy gets to act on the signal before a caller starts
/// the container that depends on it. The reload is the next thing its accept
/// loop does; this only has to outlast that.
const RELOAD_SETTLE: Duration = Duration::from_millis(250);

async fn reload_scoped_filter(gateway: &str) -> Result<(), BoundaryError> {
    podman(&["exec", gateway, "sh", "-c", RELOAD_SCOPED_FILTER]).await?;
    tokio::time::sleep(RELOAD_SETTLE).await;
    Ok(())
}

/// The same reload from a `Drop`, which cannot await. Closing does not wait
/// for the reload to settle: nothing is about to depend on it.
fn reload_scoped_filter_blocking(gateway: &str) -> Result<(), BoundaryError> {
    let out = std::process::Command::new(podman_bin())
        .args(["exec", gateway, "sh", "-c", RELOAD_SCOPED_FILTER])
        .output()
        .map_err(BoundaryError::Io)?;
    if out.status.success() {
        Ok(())
    } else {
        Err(BoundaryError::Podman(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
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

pub(crate) fn resolve_podman_env(cfg: &Config) -> String {
    let candidates = [
        PathBuf::from("/opt/homebrew/bin/podman"),
        PathBuf::from("/usr/local/bin/podman"),
        PathBuf::from("/usr/bin/podman"),
    ];
    let refs: Vec<&Path> = candidates.iter().map(PathBuf::as_path).collect();
    resolve_podman(
        &cfg.boundary.podman,
        std::env::var_os("PATH").as_deref(),
        &refs,
    )
}

/// Run `podman` with args and return stdout, or the stderr as an error.
#[async_trait]
impl super::ImageBuilder for PodmanBackend {
    async fn build(
        &self,
        build: super::ImageBuild<'_>,
    ) -> Result<super::BuiltImage, super::BuildFailure> {
        let mut command = tokio::process::Command::new(resolve_podman_env(&self.cfg));
        command.arg("build");
        for (name, value) in build.labels {
            command.arg("--label").arg(format!("{name}={value}"));
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
        let Some(image) = self.identity(build.tag).await else {
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

    async fn exists(&self, image: &str) -> bool {
        podman(&["image", "exists", image]).await.is_ok()
    }

    async fn identity(&self, image: &str) -> Option<String> {
        let digest = podman(&["image", "inspect", "--format", "{{.Digest}}", image])
            .await
            .ok()?;
        // A tag is the last `:` after the last `/`; a registry's port is not.
        let name = match image.rsplit_once(':') {
            Some((name, tag)) if !tag.contains('/') => name,
            _ => image,
        };
        let identity = format!("{name}@{}", digest.trim());
        crate::config::immutable_image(&identity)
            .is_ok()
            .then_some(identity)
    }

    async fn remove(&self, image: &str) {
        let _ = podman(&["image", "rm", image]).await;
    }
}

fn log_tail(bytes: &[u8]) -> String {
    const MAX: usize = 8192;
    let text = String::from_utf8_lossy(bytes);
    let start = text.len().saturating_sub(MAX);
    let start = (start..text.len())
        .find(|at| text.is_char_boundary(*at))
        .unwrap_or(text.len());
    text[start..].to_string()
}

pub(crate) async fn podman(args: &[&str]) -> Result<String, BoundaryError> {
    let bin = podman_bin();
    let out = tokio::process::Command::new(bin)
        .args(args)
        .output()
        .await
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                BoundaryError::Other(format!(
                    "podman not found (tried `{bin}`); install podman or set `podman = \"/path/to/podman\"` under [boundary] in node.toml"
                ))
            } else {
                BoundaryError::Io(e)
            }
        })?;
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

/// Where the podman machine stands. On macOS rootless podman runs inside a VM
/// the node is outside of, and nothing starts that VM at login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MachineState {
    Missing,
    Starting(String),
    Stopped(String),
    Running,
}

/// Read `podman machine list --format json`: the default machine, else the
/// first one listed.
pub fn machine_state_from(list: &serde_json::Value) -> MachineState {
    let machines = list.as_array().map(Vec::as_slice).unwrap_or_default();
    let Some(m) = machines
        .iter()
        .find(|m| m["Default"].as_bool() == Some(true))
        .or_else(|| machines.first())
    else {
        return MachineState::Missing;
    };
    let name = m["Name"].as_str().unwrap_or("").to_string();
    if m["Running"].as_bool() == Some(true) {
        MachineState::Running
    } else if m["Starting"].as_bool() == Some(true) {
        MachineState::Starting(name)
    } else {
        MachineState::Stopped(name)
    }
}

const MACHINE_START_TIMEOUT: Duration = Duration::from_secs(120);

/// Serializes starts: startup verification and a re-check from the interface
/// can arrive together, and a second `podman machine start` fails on the first.
static MACHINE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Make sure the podman machine is up, starting it when `start_machine` allows.
/// Off macOS podman runs on the host and there is nothing to do.
pub async fn ensure_machine(cfg: &Config) -> Result<(), BoundaryError> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    let _one = MACHINE.lock().await;
    let deadline = tokio::time::Instant::now() + MACHINE_START_TIMEOUT;
    loop {
        match machine_state_from(&podman_json(&["machine", "list", "--format", "json"]).await?) {
            MachineState::Running => return Ok(()),
            MachineState::Missing => {
                return Err(BoundaryError::Other(
                    "there is no podman machine; create one with `podman machine init`".into(),
                ))
            }
            MachineState::Stopped(name) if cfg.boundary.start_machine => {
                tokio::time::timeout(MACHINE_START_TIMEOUT, podman(&["machine", "start", &name]))
                    .await
                    .map_err(|_| {
                        BoundaryError::Other(format!(
                            "podman machine {name} did not start within two minutes"
                        ))
                    })??;
                return Ok(());
            }
            MachineState::Stopped(name) => {
                return Err(BoundaryError::Other(format!(
                    "podman machine {name} is not running; start it with `podman machine start`"
                )))
            }
            MachineState::Starting(name) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(BoundaryError::Other(format!(
                        "podman machine {name} did not finish starting within two minutes"
                    )));
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
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
    use super::{machine_state_from, resolve_podman, MachineState};
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn machine_state_reads_the_list_json() {
        let one = |running: bool, starting: bool| json!([{"Name": "podman-machine-default", "Default": true, "Running": running, "Starting": starting}]);
        let name = || "podman-machine-default".to_string();
        assert_eq!(machine_state_from(&one(true, false)), MachineState::Running);
        assert_eq!(
            machine_state_from(&one(false, true)),
            MachineState::Starting(name())
        );
        assert_eq!(
            machine_state_from(&one(false, false)),
            MachineState::Stopped(name())
        );
        assert_eq!(machine_state_from(&json!([])), MachineState::Missing);
        // The default machine is the one podman talks to, wherever it is listed.
        let two = json!([
            {"Name": "other", "Default": false, "Running": true, "Starting": false},
            {"Name": "main", "Default": true, "Running": false, "Starting": false}
        ]);
        assert_eq!(
            machine_state_from(&two),
            MachineState::Stopped("main".into())
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
}
