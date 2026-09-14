//! The Podman runner and the single place a harness `podman run` line is built.
//! The boundary checks render the same spec against a probe container, so what
//! is verified is what sessions actually run.

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
    pub network: String,
    pub gateway_host: String,
    pub gateway_ip: String,
    pub proxy_port: u16,
    pub selinux_label_disable: bool,
    pub extra_mounts: Vec<Mount>,
    pub workdir: String,
    /// The harness runs as root in its container; its state is under `/root`.
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
    pub fn from_config(cfg: &Config, selinux: bool) -> Self {
        let layout = crate::adapter::layout(&cfg.harness.id);
        Self {
            podman_bin: crate::boundary::podman::resolve_podman_env(cfg),
            image: cfg.boundary.harness_image.clone(),
            network: cfg.boundary.network.clone(),
            gateway_host: cfg.boundary.gateway_container.clone(),
            gateway_ip: cfg.boundary.gateway_ip.clone(),
            proxy_port: cfg.gateway.proxy_port,
            selinux_label_disable: cfg.boundary.selinux_label_disable.unwrap_or(selinux),
            extra_mounts: Vec::new(),
            workdir: "/work".into(),
            home: crate::session::materialize::PODMAN_HARNESS_HOME.into(),
            state_env: layout.env,
            state_dir: layout.dir,
            stop_timeout_secs: cfg.boundary.stop_timeout_secs,
        }
    }

    /// The argv for `podman run`, minus the trailing command. `detached_probe`
    /// creates the container without starting it, for inspection.
    pub fn podman_args(&self, name: &str, cmd: &RunnerCommand, create_only: bool) -> Vec<String> {
        self.podman_args_publishing(name, cmd, create_only, None)
    }

    /// As `podman_args`, publishing the command's exposed port at
    /// `127.0.0.1:<host_port>`. The bind address is not configurable on
    /// purpose: a harness server is reachable by this node and by nothing
    /// else on the network, whatever the container's own network is.
    pub fn podman_args_publishing(
        &self,
        name: &str,
        cmd: &RunnerCommand,
        create_only: bool,
        host_port: Option<u16>,
    ) -> Vec<String> {
        let proxy = format!("http://{}:{}", self.gateway_host, self.proxy_port);
        let mut a: Vec<String> = vec![
            if create_only { "create" } else { "run" }.into(),
            "--rm".into(),
            "-i".into(),
            "--name".into(),
            name.into(),
            "--network".into(),
            self.network.clone(),
            "--add-host".into(),
            format!("{}:{}", self.gateway_host, self.gateway_ip),
            // The gate: no capabilities, no way to gain any.
            "--cap-drop=ALL".into(),
            "--security-opt=no-new-privileges".into(),
            // A real init as PID 1 of the container's own PID namespace, with
            // the harness as its child. OpenCode's `serve` installs no signal
            // handlers and its LSP children are spawned without `detached`, so
            // nothing upstream reaps them (`config-state.md` §6.7): the
            // namespace is what does, and the init is what keeps a
            // double-forked formatter from being reparented to something
            // outside it in the meantime.
            "--init".into(),
            // A stop is a SIGTERM to that init and a bounded wait, not an
            // immediate SIGKILL: a harness that can flush gets the chance,
            // and one that cannot is still gone when the wait runs out.
            "--stop-signal".into(),
            "SIGTERM".into(),
            "--stop-timeout".into(),
            self.stop_timeout_secs.to_string(),
        ];
        if create_only {
            // `--rm` with `create` would delete the container before it can be
            // inspected.
            a.retain(|x| x != "--rm");
        }
        if self.selinux_label_disable {
            a.push("--security-opt".into());
            a.push("label=disable".into());
        }
        if let (Some(container_port), Some(host_port)) = (cmd.expose, host_port) {
            a.push("--publish".into());
            a.push(format!("127.0.0.1:{host_port}:{container_port}"));
        }
        // The state dir is explicit even though the image sets it: the mount
        // target and the harness's idea of its state directory must agree. A
        // harness that names no such variable (OpenCode decides by HOME and
        // XDG, which its adapter sets) gets none invented for it.
        let state = format!("{}/{}", self.home, self.state_dir);
        for (k, v) in [
            ("HTTPS_PROXY", proxy.as_str()),
            ("HTTP_PROXY", proxy.as_str()),
            ("NO_PROXY", self.gateway_host.as_str()),
            (self.state_env, state.as_str()),
        ] {
            // A value the command sets itself is not also set here: two `-e`
            // entries for one name is a rule about ordering rather than a
            // choice.
            if k.is_empty() || cmd.env.iter().any(|(name, _)| name == k) {
                continue;
            }
            a.push("-e".into());
            a.push(format!("{k}={v}"));
        }
        for (k, v) in &cmd.env {
            a.push("-e".into());
            a.push(format!("{k}={v}"));
        }
        for m in self.extra_mounts.iter().chain(cmd.mounts.iter()) {
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
        // Podman 6.1 rejects an explicit `-w` whose path is only in the image
        // layer ("workdir does not exist on container"), so the flag is passed
        // only when a mount actually provides that directory. Otherwise the
        // image's own WORKDIR applies, which is the same path.
        let workdir = cmd.workdir.clone().unwrap_or_else(|| self.workdir.clone());
        let mounted = self
            .extra_mounts
            .iter()
            .chain(cmd.mounts.iter())
            .any(|m| m.target == workdir);
        if mounted {
            a.push("-w".into());
            a.push(workdir);
        }
        a.push(cmd.image.clone().unwrap_or_else(|| self.image.clone()));
        a.extend(cmd.argv.iter().cloned());
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

    async fn ensure_volumes(&self, cmd: &RunnerCommand) -> Result<(), RunnerError> {
        let mut seen = std::collections::BTreeSet::new();
        for mount in self.spec.extra_mounts.iter().chain(cmd.mounts.iter()) {
            if !seen.insert(&mount.volume) {
                continue;
            }
            let out = Command::new(&self.spec.podman_bin)
                .args(["volume", "create", "--ignore", &mount.volume])
                .output()
                .await?;
            if !out.status.success() {
                return Err(RunnerError::Other(format!(
                    "create runtime volume {}: {}",
                    mount.volume,
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Runner for PodmanRunner {
    async fn spawn(&self, cmd: RunnerCommand) -> Result<Spawned, RunnerError> {
        self.ensure_volumes(&cmd).await?;
        let name = if cmd.name.is_empty() {
            "tracon-h".to_string()
        } else {
            cmd.name.clone()
        };
        // A harness driven over HTTP gets its port published to this host's
        // loopback and nowhere else. The port is picked here rather than in
        // the adapter because it is the host's, not the container's.
        let host_port = match cmd.expose {
            Some(_) => Some(super::free_loopback_port()?),
            None => None,
        };
        let args = self
            .spec
            .podman_args_publishing(&name, &cmd, false, host_port);
        tracing::debug!(container = %name, "podman run");
        let child = Command::new(&self.spec.podman_bin)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        Ok(Spawned::from_child(child)?.at(host_port.map(|port| format!("127.0.0.1:{port}"))))
    }

    async fn run_capture(&self, cmd: RunnerCommand) -> Result<std::process::Output, RunnerError> {
        self.ensure_volumes(&cmd).await?;
        let name = self.capture_name(&cmd.name);
        let args = self.spec.podman_args(&name, &cmd, false);
        Command::new(&self.spec.podman_bin)
            .args(&args)
            .output()
            .await
            .map_err(Into::into)
    }

    /// Stop, then remove. `podman rm --force` on its own is a SIGKILL: the
    /// container's processes are gone either way, but nothing gets to finish
    /// writing. `stop` sends the container's stop signal to its init and waits
    /// `--stop-timeout` before escalating, and the removal afterwards is what
    /// tears the PID namespace down, so no harness, LSP or formatter process
    /// can outlive this call whichever path it took.
    async fn kill(&self, name: &str) -> Result<(), RunnerError> {
        let timeout = self.spec.stop_timeout_secs.to_string();
        let _ = Command::new(&self.spec.podman_bin)
            .args(["stop", "--time", &timeout, "--ignore", name])
            .output()
            .await;
        let _ = Command::new(&self.spec.podman_bin)
            .args(["rm", "-f", "-i", name])
            .output()
            .await?;
        Ok(())
    }

    /// Two node processes can share a container runtime, so a capture's
    /// container carries this process's id. `kill` has to be given the same
    /// name back.
    fn capture_name(&self, name: &str) -> String {
        format!(
            "{}-{}",
            if name.is_empty() { "tracon-x" } else { name },
            std::process::id()
        )
    }

    /// Ask podman itself for the digest of `spec.image` rather than trusting
    /// the configured string: podman resolves exactly this reference to
    /// whatever it has cached locally under that name, and that resolution —
    /// not the operator's text — is what every `podman run` with this spec
    /// actually executes. `None` on any failure (image absent, no digest
    /// recorded, podman unreachable): evidence must never invent a pin.
    async fn resolved_image(&self) -> Option<String> {
        let output = Command::new(&self.spec.podman_bin)
            .args([
                "inspect",
                "--format",
                "{{index .RepoDigests 0}}",
                &self.spec.image,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .await
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let digest = String::from_utf8(output.stdout).ok()?;
        let digest = digest.trim();
        (!digest.is_empty() && digest.contains("@sha256:")).then(|| digest.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> RunSpec {
        RunSpec::from_config(&Config::default(), false)
    }

    #[test]
    fn run_line_carries_the_gate() {
        let args = spec().podman_args(
            "tracon-h-1",
            &RunnerCommand {
                argv: vec!["opencode".into(), "serve".into()],
                ..Default::default()
            },
            false,
        );
        let joined = args.join(" ");
        assert!(joined.contains("--cap-drop=ALL"));
        assert!(joined.contains("--security-opt=no-new-privileges"));
        // A PID namespace with a real init at its head, and a stop that is a
        // signal before it is a kill.
        assert!(joined.contains("--init"));
        assert!(joined.contains("--stop-signal SIGTERM"));
        assert!(joined.contains("--stop-timeout 10"));
        assert!(joined.contains("--network tracon-int"));
        assert!(joined.contains("HTTPS_PROXY=http://tracon-gw:8888"));
        // OpenCode names no state-directory variable, so the runner sets
        // none rather than inventing one; where it keeps things is decided by
        // the HOME and XDG values its adapter passes per session.
        assert!(!joined.contains("STATE_DIR="));
        assert!(joined.ends_with("localhost/tracon-harness-opencode opencode serve"));
        // No SELinux flag unless the host needs it.
        assert!(!joined.contains("label=disable"));
        // Nothing is mounted at /work here, so the image's WORKDIR is used.
        assert!(!joined.contains(" -w "));
    }

    #[test]
    fn probe_container_is_created_not_removed() {
        let args = spec().podman_args("probe", &RunnerCommand::default(), true);
        assert_eq!(args[0], "create");
        assert!(
            !args.iter().any(|a| a == "--rm"),
            "probe must survive inspect"
        );
    }

    #[test]
    fn selinux_hosts_get_label_disable() {
        let mut s = spec();
        s.selinux_label_disable = true;
        let args = s.podman_args("probe", &RunnerCommand::default(), false);
        assert!(args
            .windows(2)
            .any(|w| w == ["--security-opt", "label=disable"]));
    }

    #[test]
    fn mounts_render_with_read_only_flag() {
        let mut s = spec();
        s.extra_mounts
            .push(Mount::volume("tracon-state", "/root/.omp", false));
        let args = s.podman_args(
            "probe",
            &RunnerCommand {
                mounts: vec![Mount::volume("tracon-work", "/work", true)],
                ..Default::default()
            },
            false,
        );
        let joined = args.join(" ");
        assert!(joined.contains("--mount type=volume,src=tracon-state,dst=/root/.omp"));
        assert!(joined.contains("--mount type=volume,src=tracon-work,dst=/work,ro"));
        // A mount provides /work, so the explicit workdir is safe to pass.
        assert!(joined.contains("-w /work"));
    }
}
