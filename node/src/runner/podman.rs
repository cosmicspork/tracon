//! What a Podman sandbox container is made from, and the single place its
//! `podman run` line is built (`RunSpec::sandbox_args`); the boundary checks
//! render the same spec against a probe container, so what is verified is
//! what sessions actually run. `PodmanRunner` is what a sandbox's runner
//! hands on: an image's identity.

use std::process::Stdio;

use async_trait::async_trait;
use tokio::process::Command;

use super::{Mount, Runner, RunnerCommand, RunnerError, Spawned};
use crate::config::Config;

/// Everything that puts a harness process inside the boundary.
#[derive(Debug, Clone)]
pub struct RunSpec {
    /// The podman binary, resolved once from config/PATH/known locations.
    pub podman_bin: String,
    pub image: String,
    /// The name a harness reaches the gateways by: its own loopback, where
    /// the tunnels listen.
    pub gateway_host: String,
    pub proxy_port: u16,
    pub extra_mounts: Vec<Mount>,
    pub workdir: String,
    /// The harness's home in the sandbox; its state is under it.
    pub home: String,
    /// What this harness calls its state directory. The mount target and the
    /// harness's own idea of where its state lives have to agree, and the name
    /// of the variable that says so is the harness's, not ours.
    pub state_env: &'static str,
    pub state_dir: &'static str,
    /// Seconds a stopped container gets between SIGTERM and SIGKILL.
    pub stop_timeout_secs: u16,
}

impl RunSpec {
    pub fn from_config(cfg: &Config) -> Self {
        Self::for_harness(cfg, &cfg.harness.id)
    }

    /// The spec for a named harness: its image, its state layout, everything
    /// else the boundary's.
    pub fn for_harness(cfg: &Config, harness_id: &str) -> Self {
        let layout = crate::adapter::layout(harness_id);
        Self {
            podman_bin: crate::boundary::podman::resolve_podman_env(cfg),
            image: crate::sandbox_images::base_image(cfg),
            gateway_host: cfg.boundary.gateway_container.clone(),
            proxy_port: cfg.gateway.proxy_port,
            extra_mounts: Vec::new(),
            workdir: "/work".into(),
            home: crate::sandbox_images::HOME.into(),
            state_env: layout.env,
            state_dir: layout.dir,
            stop_timeout_secs: cfg.boundary.stop_timeout_secs,
        }
    }
}

/// How `tracon-sandbox` starts in its container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxEntry {
    /// As the container's entrypoint, PID 1 of its namespace: it reaps
    /// orphans and turns the stop signal into its own teardown.
    Pid1,
    /// Under Podman's `--init`, as that init's child and the subreaper of
    /// everything it starts.
    UnderInit,
    /// As the command of an image whose own entrypoint must run first (it
    /// prepares the environment and `exec`s its arguments, typically).
    UnderImageEntrypoint,
}

/// What a sandbox container carries beyond a harness container: the bundle
/// and its supervisor, the directory holding the node's socket, and the
/// supervisor's bootstrap. The token is not here: it is passed as
/// `--env TRACON_SANDBOX_TOKEN` with its value in podman's own environment,
/// as a command's secret variables are.
#[derive(Debug, Clone)]
pub struct SandboxLaunch {
    pub bundle: std::path::PathBuf,
    /// The node's listener is `node.sock` in here.
    pub socket_dir: std::path::PathBuf,
    /// `sha256:<hex>` of the node's certificate.
    pub pin: String,
    pub entry: SandboxEntry,
    pub cpu_millis: Option<u32>,
    pub memory_bytes: Option<u64>,
    /// How long the supervisor keeps dialling a node not yet listening.
    pub connect_timeout_ms: u64,
    /// Where the container's own output (the supervisor's log) goes, so it
    /// outlives the container.
    pub log_file: Option<std::path::PathBuf>,
    /// The user the container runs as, where not the image's own.
    pub user: Option<String>,
    /// The user namespace it runs in, where not Podman's default.
    pub userns: Option<String>,
    /// Environment the image does not set and the sandbox needs.
    pub env: Vec<(String, String)>,
    /// Catalogue service bundles, each mounted read-only at its target
    /// (`/opt/tracon-services/<name>`), for the supervisor to run.
    pub service_bundles: Vec<(std::path::PathBuf, String)>,
}

/// Where the bundle is mounted inside a sandbox.
pub const BUNDLE_TARGET: &str = "/opt/tracon";
/// The supervisor, inside the bundle.
pub const SANDBOX_BIN: &str = "/opt/tracon/bin/tracon-sandbox";
/// Where the socket directory is mounted inside a sandbox.
pub const SOCKET_DIR_TARGET: &str = "/run/tracon";
/// The node's socket, inside.
pub const SOCKET_INSIDE: &str = "/run/tracon/node.sock";
/// Marks the containers the sandbox provider made.
pub const SANDBOX_LABEL: &str = "tracon.provider=podman";

impl RunSpec {
    /// The argv for a sandbox container: no capabilities and no network,
    /// the boundary's environment, and `tracon-sandbox` from the mounted
    /// bundle as what it runs; the harness it starts later over the
    /// protocol. Detached and removed when it exits, as a lost
    /// connection ends it; its log is written to `launch.log_file`, which the
    /// provider removes with the sandbox. `create_only` creates it without
    /// starting it, for the boundary checks' probe.
    ///
    /// Never `label=disable`: the bundle is relabelled shared (readable by
    /// every container, writable through this mount by none), the socket
    /// directory is labelled by the node, and the named volumes carry
    /// Podman's own shared label.
    pub fn sandbox_args(
        &self,
        name: &str,
        image: &str,
        mounts: &[Mount],
        launch: &SandboxLaunch,
        create_only: bool,
    ) -> Vec<String> {
        let mut a: Vec<String> = vec![if create_only { "create" } else { "run" }.into()];
        if !create_only {
            a.push("--detach".into());
            a.push("--rm".into());
        }
        // Node-only: no interface but loopback, so the protocol's socket is
        // the one way out, and the gateway's name is loopback, where the
        // tunnels stand in for it on the ports it used.
        a.extend([
            "--name".into(),
            name.into(),
            "--label".into(),
            SANDBOX_LABEL.into(),
            "--network".into(),
            "none".into(),
            "--add-host".into(),
            format!("{}:127.0.0.1", self.gateway_host),
            "--cap-drop=ALL".into(),
            "--security-opt=no-new-privileges".into(),
        ]);
        if launch.entry == SandboxEntry::UnderInit {
            a.push("--init".into());
        }
        a.extend([
            "--stop-signal".into(),
            "SIGTERM".into(),
            "--stop-timeout".into(),
            self.stop_timeout_secs.to_string(),
        ]);
        let proxy = format!("http://{}:{}", self.gateway_host, self.proxy_port);
        let direct = super::no_proxy(&self.gateway_host);
        let state = format!("{}/{}", self.home, self.state_dir);
        for (k, v) in [
            ("HTTPS_PROXY", proxy.as_str()),
            ("HTTP_PROXY", proxy.as_str()),
            ("NO_PROXY", direct.as_str()),
            ("no_proxy", direct.as_str()),
            (self.state_env, state.as_str()),
            ("TRACON_SANDBOX_SOCKET", SOCKET_INSIDE),
            ("TRACON_SANDBOX_PIN", launch.pin.as_str()),
        ] {
            if k.is_empty() {
                continue;
            }
            a.push("-e".into());
            a.push(format!("{k}={v}"));
        }
        a.push("-e".into());
        a.push(format!(
            "TRACON_SANDBOX_CONNECT_TIMEOUT_MS={}",
            launch.connect_timeout_ms
        ));
        for (k, v) in &launch.env {
            a.push("-e".into());
            a.push(format!("{k}={v}"));
        }
        if let Some(userns) = &launch.userns {
            a.push(format!("--userns={userns}"));
        }
        if let Some(user) = &launch.user {
            a.push("--user".into());
            a.push(user.clone());
        }
        a.push("--env".into());
        a.push("TRACON_SANDBOX_TOKEN".into());
        if let Some(log) = &launch.log_file {
            a.push("--log-driver".into());
            a.push("k8s-file".into());
            a.push("--log-opt".into());
            a.push(format!("path={}", log.display()));
        }
        a.push("--mount".into());
        a.push(format!(
            "type=bind,src={},dst={BUNDLE_TARGET},ro,relabel=shared",
            launch.bundle.display()
        ));
        for (bundle, target) in &launch.service_bundles {
            a.push("--mount".into());
            a.push(format!(
                "type=bind,src={},dst={target},ro,relabel=shared",
                bundle.display()
            ));
        }
        // Read-only: the supervisor dials the socket, and nothing inside may
        // put another in its place.
        a.push("--mount".into());
        a.push(format!(
            "type=bind,src={},dst={SOCKET_DIR_TARGET},ro",
            launch.socket_dir.display()
        ));
        for m in self.extra_mounts.iter().chain(mounts.iter()) {
            if m.is_local() {
                // Anonymous, so `--rm` takes it with the container; owned by
                // the image's user, so the supervisor can write into it.
                a.push("--mount".into());
                a.push(format!("type=volume,dst={},U=true", m.target));
                continue;
            }
            let mut mount = format!("type=volume,src={},dst={}", m.volume, m.target);
            if !m.sub_path.is_empty() {
                mount.push_str(",volume-subpath=");
                mount.push_str(&m.sub_path);
            }
            if m.read_only {
                mount.push_str(",ro");
            }
            a.push("--mount".into());
            a.push(mount);
        }
        if let Some(millis) = launch.cpu_millis {
            a.push("--cpus".into());
            a.push(format!("{:.3}", f64::from(millis) / 1000.0));
        }
        if let Some(bytes) = launch.memory_bytes {
            a.push("--memory".into());
            a.push(bytes.to_string());
        }
        if crate::boundary::podman::local_only(image) {
            a.push("--pull=never".into());
        }
        match launch.entry {
            SandboxEntry::Pid1 | SandboxEntry::UnderInit => {
                a.push("--entrypoint".into());
                a.push(SANDBOX_BIN.into());
                a.push(image.into());
            }
            SandboxEntry::UnderImageEntrypoint => {
                a.push(image.into());
                a.push(SANDBOX_BIN.into());
            }
        }
        a
    }
}

pub struct PodmanRunner {
    spec: RunSpec,
}

impl PodmanRunner {
    pub fn new(spec: RunSpec) -> Self {
        Self { spec }
    }

    pub fn spec(&self) -> &RunSpec {
        &self.spec
    }

    /// `podman`, checked by the path guard just before it is spawned.
    fn podman(&self) -> std::io::Result<Command> {
        let mut command = Command::new(crate::config::path_guard::program(
            "boundary.podman",
            &self.spec.podman_bin,
        )?);
        command.env("PATH", crate::config::path_guard::child_path());
        Ok(command)
    }
}

/// What a Podman node runs a command in: always a sandbox, made by the
/// provider (`boundary::podman::protocol_runner`).
const NOT_A_SANDBOX: &str =
    "a Podman node runs every command in a sandbox over the sandbox protocol";

#[async_trait]
impl Runner for PodmanRunner {
    async fn spawn(&self, _cmd: RunnerCommand) -> Result<Spawned, RunnerError> {
        Err(RunnerError::Other(NOT_A_SANDBOX.into()))
    }

    async fn run_capture(&self, _cmd: RunnerCommand) -> Result<std::process::Output, RunnerError> {
        Err(RunnerError::Other(NOT_A_SANDBOX.into()))
    }

    /// Stop, then remove. `podman rm --force` on its own is a SIGKILL: the
    /// container's processes are gone either way, but nothing gets to finish
    /// writing. `stop` sends the container's stop signal to its init and waits
    /// `--stop-timeout` before escalating, and the removal afterwards is what
    /// tears the PID namespace down, so no harness, LSP or formatter process
    /// can outlive this call whichever path it took.
    async fn kill(&self, name: &str) -> Result<(), RunnerError> {
        let timeout = self.spec.stop_timeout_secs.to_string();
        let _ = self
            .podman()?
            .args(["stop", "--time", &timeout, "--ignore", name])
            .output()
            .await;
        let _ = self
            .podman()?
            .args(["rm", "-f", "-i", name])
            .output()
            .await?;
        Ok(())
    }

    /// Ask podman itself for the digest of the image this command will run in
    /// rather than trusting a configured string: podman resolves exactly that
    /// reference to whatever it has cached locally under that name, and that
    /// resolution — not the operator's text — is what `podman run` actually
    /// executes. `image` is a command's own override (a project toolchain
    /// image, a preparation image); without one this is the runner's spec
    /// image, as before. `None` on any failure (image absent, no digest
    /// recorded, podman unreachable): evidence must never invent a pin.
    async fn resolved_image(&self, image: Option<&str>) -> Option<String> {
        let image = image.unwrap_or(&self.spec.image);
        let output = self
            .podman()
            .ok()?
            .args(["inspect", "--format", "{{json .RepoDigests}}", image])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .await
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let digests: Vec<String> = serde_json::from_slice(&output.stdout).ok()?;
        repo_digest_for(image, &digests)
    }
}

/// One image built from one Dockerfile under two names carries a digest name
/// per tag, in no order that says which was asked for. The evidence names the
/// one under the requested repository; the first entry stands in only when
/// none is (a short name podman qualified, say).
fn repo_digest_for(image: &str, digests: &[String]) -> Option<String> {
    let repository = image_repository(image);
    digests
        .iter()
        .find(|digest| image_repository(digest) == repository)
        .or_else(|| digests.first())
        .map(|digest| digest.trim())
        .filter(|digest| digest.contains("@sha256:"))
        .map(str::to_string)
}

/// A reference without its digest or tag. A colon before the last `/` is a
/// registry port, not a tag.
fn image_repository(image: &str) -> &str {
    let name = image.split_once('@').map_or(image, |(name, _)| name);
    match name.rfind(':') {
        Some(colon) if !name[colon..].contains('/') => &name[..colon],
        _ => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> RunSpec {
        RunSpec::from_config(&Config::default())
    }

    #[test]
    fn the_recorded_digest_name_is_the_requested_images_whichever_tag_podman_lists_first() {
        let digest = format!("sha256:{}", "9e".repeat(32));
        let ours = format!("localhost/tracon-repo-hound-d157e849da@{digest}");
        let theirs = format!("localhost/tracon-repo-hound-4b9bd93690@{digest}");
        for digests in [
            vec![theirs.clone(), ours.clone()],
            vec![ours.clone(), theirs.clone()],
        ] {
            assert_eq!(
                repo_digest_for("localhost/tracon-repo-hound-d157e849da:latest", &digests),
                Some(ours.clone())
            );
        }
        // Nothing under the requested name: the first entry, as before.
        assert_eq!(
            repo_digest_for("alpine", &[theirs.clone(), ours.clone()]),
            Some(theirs)
        );
        assert_eq!(repo_digest_for("alpine", &[]), None);
        assert_eq!(
            image_repository("registry:5000/a/b:1@sha256:x"),
            "registry:5000/a/b"
        );
        assert_eq!(image_repository("registry:5000/a/b"), "registry:5000/a/b");
    }

    /// Nothing runs as a plain container on a Podman node: a sandbox's
    /// runner is the provider's, and this one only ever answers for images.
    #[tokio::test]
    async fn a_command_is_never_run_outside_a_sandbox() {
        let runner = PodmanRunner::new(spec());
        let error = runner
            .run_capture(RunnerCommand::default())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("in a sandbox"), "{error}");
        assert!(runner.spawn(RunnerCommand::default()).await.is_err());
    }

    /// What only this host's store can hold is run without pulling, so one
    /// that is gone fails as missing instead of asking `localhost` for it.
    #[test]
    fn a_local_image_is_never_pulled_for_a_sandbox() {
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
        let run = |image: &str| spec().sandbox_args("tracon-c-1", image, &[], &launch, false);
        let never = "--pull=never".to_string();
        assert!(run("localhost/tracon-repo-app-1a2b@sha256:aa").contains(&never));
        assert!(!run("ghcr.io/acme/app@sha256:aa").contains(&never));
        // No network, and the gateway's name is loopback.
        let line = run("img").join(" ");
        assert!(
            line.contains("--network none --add-host tracon-gw:127.0.0.1"),
            "{line}"
        );
    }
}
