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
}

impl RunSpec {
    pub fn from_config(cfg: &Config, selinux: bool) -> Self {
        let layout = crate::adapter::layout(&cfg.harness.id);
        Self {
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
        }
    }

    /// The argv for `podman run`, minus the trailing command. `detached_probe`
    /// creates the container without starting it, for inspection.
    pub fn podman_args(&self, name: &str, cmd: &RunnerCommand, create_only: bool) -> Vec<String> {
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
        // The state dir is explicit even though the image sets it: the mount
        // target and the harness's idea of its state directory must agree.
        let state = format!("{}/{}", self.home, self.state_dir);
        for (k, v) in [
            ("HTTPS_PROXY", proxy.as_str()),
            ("HTTP_PROXY", proxy.as_str()),
            ("NO_PROXY", self.gateway_host.as_str()),
            (self.state_env, state.as_str()),
        ] {
            a.push("-e".into());
            a.push(format!("{k}={v}"));
        }
        for (k, v) in &cmd.env {
            a.push("-e".into());
            a.push(format!("{k}={v}"));
        }
        for m in self.extra_mounts.iter().chain(cmd.mounts.iter()) {
            a.push("-v".into());
            a.push(format!(
                "{}:{}{}",
                m.source,
                m.target,
                if m.read_only { ":ro" } else { "" }
            ));
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
        a.push(self.image.clone());
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
}

#[async_trait]
impl Runner for PodmanRunner {
    async fn spawn(&self, cmd: RunnerCommand) -> Result<Spawned, RunnerError> {
        let name = if cmd.name.is_empty() {
            "tracon-h".to_string()
        } else {
            cmd.name.clone()
        };
        let args = self.spec.podman_args(&name, &cmd, false);
        tracing::debug!(?args, "podman run");
        let child = Command::new("podman")
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        Spawned::from_child(child)
    }

    async fn run_capture(&self, cmd: RunnerCommand) -> Result<std::process::Output, RunnerError> {
        let name = format!(
            "{}-{}",
            if cmd.name.is_empty() {
                "tracon-x"
            } else {
                &cmd.name
            },
            std::process::id()
        );
        let args = self.spec.podman_args(&name, &cmd, false);
        Command::new("podman")
            .args(&args)
            .output()
            .await
            .map_err(Into::into)
    }

    async fn kill(&self, name: &str) -> Result<(), RunnerError> {
        let _ = Command::new("podman")
            .args(["rm", "-f", "-i", name])
            .output()
            .await?;
        Ok(())
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
                argv: vec!["omp".into(), "acp".into()],
                ..Default::default()
            },
            false,
        );
        let joined = args.join(" ");
        assert!(joined.contains("--cap-drop=ALL"));
        assert!(joined.contains("--security-opt=no-new-privileges"));
        assert!(joined.contains("--network tracon-int"));
        assert!(joined.contains("HTTPS_PROXY=http://tracon-gw:8888"));
        assert!(joined.contains("OMP_STATE_DIR=/root/.omp"));
        assert!(joined.ends_with("localhost/tracon-harness omp acp"));
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
        s.extra_mounts.push(Mount {
            source: "/state".into(),
            target: "/root/.omp".into(),
            read_only: false,
        });
        let args = s.podman_args(
            "probe",
            &RunnerCommand {
                mounts: vec![Mount {
                    source: "/wt".into(),
                    target: "/work".into(),
                    read_only: true,
                }],
                ..Default::default()
            },
            false,
        );
        let joined = args.join(" ");
        assert!(joined.contains("-v /state:/root/.omp "));
        assert!(joined.contains("-v /wt:/work:ro"));
        // A mount provides /work, so the explicit workdir is safe to pass.
        assert!(joined.contains("-w /work"));
    }
}
