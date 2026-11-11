//! What stands between this machine and a node the service runs: for the
//! setup page the app opens on before one exists, and for Settings after.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{node, service};

/// Who runs the node that answers on the node's address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Owner {
    /// Nothing answers.
    None,
    Service,
    /// A node an earlier version of this app spawned as its own child.
    Migrated,
    /// Someone else's: a terminal, most likely.
    Foreign,
}

/// `unit_has_process` rather than "the unit is active": across a restart the
/// unit is deactivating or activating while its node still answers, and that
/// node is the service's. A unit with no process at all — stopped, or waiting
/// to restart one that exited — cannot be what answers.
pub fn owner(answering: bool, migrated: bool, unit_has_process: bool) -> Owner {
    match (answering, migrated, unit_has_process) {
        (false, _, _) => Owner::None,
        (true, true, _) => Owner::Migrated,
        (true, false, true) => Owner::Service,
        (true, false, false) => Owner::Foreign,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SetupStatus {
    pub platform: &'static str,
    pub owner: Owner,
    pub node_version: Option<String>,
    pub sidecar_version: Option<String>,
    pub cli_version: Option<String>,
    pub cli_path: Option<String>,
    pub path_hint: Option<String>,
    pub service_installed: bool,
    pub service_running: bool,
    /// The unit is installed and its node keeps exiting.
    pub service_failing: bool,
    /// What the node said as it exited, when it is failing.
    pub service_error: Option<String>,
    /// Linux only: rootless Podman, the boundary there.
    pub podman: Option<String>,
    /// macOS only: Apple's `container` CLI, the boundary there.
    pub container: Option<String>,
    /// macOS only: `running`, `stopped` or `error`, as
    /// `container system status` answers.
    pub services: Option<&'static str>,
    /// Why `container system status` said they are not running, or could
    /// not be asked.
    pub services_error: Option<String>,
}

/// Podman as the node will find it: the PATH a login shell would add, then
/// the usual install locations.
fn find_podman() -> Option<PathBuf> {
    let path = node::enriched_path(std::env::var("PATH").ok());
    std::env::split_paths(&path)
        .map(|d| d.join("podman"))
        .chain(["/usr/local/bin/podman", "/usr/bin/podman"].map(PathBuf::from))
        .find(|p| p.is_file())
}

/// Apple's `container` as the node will find it: where its installer puts
/// it, which is where the node looks when `[runtime.apple] container` is
/// empty, then the PATH a login shell would add.
fn find_container() -> Option<PathBuf> {
    let path = node::enriched_path(std::env::var("PATH").ok());
    std::iter::once(PathBuf::from("/usr/local/bin/container"))
        .chain(std::env::split_paths(&path).map(|d| d.join("container")))
        .find(|p| p.is_file())
}

/// `container system status`: zero when its services run. A non-zero exit
/// is them stopped, with what it said; a CLI that will not run is an error.
async fn services_state(container: &Path) -> (&'static str, Option<String>) {
    let out = match tokio::process::Command::new(container)
        .args(["system", "status"])
        .stdin(std::process::Stdio::null())
        .output()
        .await
    {
        Ok(out) => out,
        Err(e) => {
            return (
                "error",
                Some(format!("Could not run `container system status`: {e}")),
            )
        }
    };
    services_state_from(out.status.success(), &out.stdout, &out.stderr)
}

fn services_state_from(
    success: bool,
    stdout: &[u8],
    stderr: &[u8],
) -> (&'static str, Option<String>) {
    if success {
        return ("running", None);
    }
    let said = [stderr, stdout]
        .iter()
        .map(|bytes| String::from_utf8_lossy(bytes).trim().to_string())
        .find(|text| !text.is_empty());
    ("stopped", said)
}

/// Why the service cannot be installed yet, if it cannot: the boundary the
/// node runs every sandbox in is missing or not running.
fn boundary_failure(status: &SetupStatus) -> Option<String> {
    if status.platform != "macos" {
        return status.podman.is_none().then(|| {
            "Podman is required before installing the service. Install Podman, then retry setup."
                .into()
        });
    }
    if status.container.is_none() {
        return Some(
            "Apple's container is required before installing the service. Install its signed \
             package, then retry setup."
                .into(),
        );
    }
    match status.services {
        Some("running") => None,
        _ => Some(
            "container's services are not running. Start them with \
             `container system start --enable-kernel-install`, then retry setup."
                .into(),
        ),
    }
}

pub async fn status(http: &reqwest::Client, url: &str) -> SetupStatus {
    let answering = node::answering(http, url).await;
    let node_version = if answering {
        node::running_version(http, url).await
    } else {
        None
    };
    let migrated = node::migrated_node(&node::state_dir()).is_some();
    let unit = service::state();
    let service_installed = service::installed();
    let cli = node::installed_path();
    let path_hint = match cli.as_deref().and_then(Path::parent) {
        Some(dir) => node::path_hint(dir).await,
        None => None,
    };
    let mac = cfg!(target_os = "macos");
    let podman = if mac { None } else { find_podman() };
    let container = if mac { find_container() } else { None };
    let (services, services_error) = match &container {
        Some(bin) => {
            let (state, error) = services_state(bin).await;
            (Some(state), error)
        }
        None => (None, None),
    };
    SetupStatus {
        platform: if cfg!(target_os = "macos") {
            "macos"
        } else {
            "linux"
        },
        owner: owner(answering, migrated, unit.has_process),
        node_version,
        sidecar_version: node::sidecar_path().and_then(|p| node::version_of(&p)),
        cli_version: cli.as_deref().and_then(node::version_of),
        cli_path: cli.map(|p| p.display().to_string()),
        path_hint,
        service_installed,
        service_running: unit.running,
        service_failing: service_installed && unit.failing,
        service_error: if service_installed {
            service::failure(unit)
        } else {
            None
        },
        podman: podman.map(|p| p.display().to_string()),
        container: container.map(|p| p.display().to_string()),
        services,
        services_error,
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| e.to_string())?
}

/// Put the node under the service: stop a node an earlier version of this
/// app runs, install the CLI the unit will name, install the unit, and wait
/// for the node to answer.
pub async fn install_service(http: &reqwest::Client, url: &str) -> Result<SetupStatus, String> {
    let initial = status(http, url).await;
    if let Some(reason) = boundary_failure(&initial) {
        return Err(reason);
    }
    match initial.owner {
        Owner::Foreign => {
            return Err(
                "a node you started yourself is answering; stop it, then install the service"
                    .into(),
            )
        }
        Owner::Migrated => {
            blocking(|| {
                node::stop_migrated_node(&node::state_dir());
                Ok(())
            })
            .await?
        }
        Owner::None | Owner::Service => {}
    }
    let cli = blocking(node::install_cli).await?;
    let mark = service::log_mark();
    blocking(move || service::install(&cli)).await?;
    if !node::wait_ready(http, url).await {
        let why = "the service started, but the node did not answer within a minute";
        return Err(match service::error_since(mark) {
            Some(reason) => format!("{why}: {reason}"),
            None => format!("{why}; `tracon service status` says why"),
        });
    }
    Ok(status(http, url).await)
}

pub async fn install_cli(http: &reqwest::Client, url: &str) -> Result<SetupStatus, String> {
    blocking(node::install_cli).await?;
    Ok(status(http, url).await)
}

pub async fn restart_node(http: &reqwest::Client, url: &str) -> Result<SetupStatus, String> {
    let cli = node::installed_path().ok_or("HOME is not set")?;
    let mark = service::log_mark();
    blocking(move || service::restart(&cli)).await?;
    if !node::wait_ready(http, url).await {
        return Err(service::explain(
            "the node did not answer within a minute of restarting".into(),
            service::error_since(mark),
        ));
    }
    Ok(status(http, url).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_owner_is_whoever_runs_what_answers() {
        assert_eq!(owner(false, true, true), Owner::None);
        // The app's own old child holds the port even when a unit exists.
        assert_eq!(owner(true, true, true), Owner::Migrated);
        assert_eq!(owner(true, false, true), Owner::Service);
        assert_eq!(owner(true, false, false), Owner::Foreign);
        // A unit that is restarting still has its node: answering then is
        // the service answering, whatever `is-active` says at that instant.
        let restarting = service::UnitState {
            running: false,
            has_process: true,
            failing: false,
        };
        assert_eq!(owner(true, false, restarting.has_process), Owner::Service);
    }

    #[test]
    fn the_services_state_is_what_container_system_status_exits_with() {
        assert_eq!(
            services_state_from(true, b"apiserver is running", b""),
            ("running", None)
        );
        assert_eq!(
            services_state_from(false, b"", b"apiserver is not running\n"),
            ("stopped", Some("apiserver is not running".into()))
        );
        assert_eq!(
            services_state_from(false, b"not registered\n", b"  "),
            ("stopped", Some("not registered".into()))
        );
        assert_eq!(services_state_from(false, b"", b""), ("stopped", None));
    }

    fn mac() -> SetupStatus {
        SetupStatus {
            platform: "macos",
            owner: Owner::None,
            node_version: None,
            sidecar_version: None,
            cli_version: None,
            cli_path: None,
            path_hint: None,
            service_installed: false,
            service_running: false,
            service_failing: false,
            service_error: None,
            podman: None,
            container: Some("/usr/local/bin/container".into()),
            services: Some("running"),
            services_error: None,
        }
    }

    #[test]
    fn service_install_on_a_mac_needs_container_and_its_services_running() {
        let mut status = mac();
        assert_eq!(boundary_failure(&status), None);
        // Podman is not what a Mac runs, so it is never asked for there.
        assert!(status.podman.is_none());
        status.services = Some("stopped");
        assert!(boundary_failure(&status)
            .unwrap()
            .contains("container system start --enable-kernel-install"));
        status.services = Some("error");
        assert!(boundary_failure(&status).is_some());
        status.container = None;
        status.services = None;
        assert!(boundary_failure(&status)
            .unwrap()
            .contains("Apple's container is required"));
    }

    #[test]
    fn service_install_on_linux_needs_podman() {
        let mut status = SetupStatus {
            platform: "linux",
            container: None,
            services: None,
            ..mac()
        };
        assert!(boundary_failure(&status).unwrap().contains("Podman"));
        status.podman = Some("/usr/bin/podman".into());
        assert_eq!(boundary_failure(&status), None);
    }
}
