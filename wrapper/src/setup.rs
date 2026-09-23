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
    pub podman: Option<String>,
    /// macOS only: `running`, `starting`, `stopped`, `missing` or `error`.
    pub machine: Option<&'static str>,
    /// Why Podman could not determine the macOS machine state.
    pub machine_error: Option<String>,
}

/// Podman as the node will find it: the PATH a login shell would add, then
/// the usual install locations.
fn find_podman() -> Option<PathBuf> {
    let path = node::enriched_path(std::env::var("PATH").ok());
    std::env::split_paths(&path)
        .map(|d| d.join("podman"))
        .chain(
            [
                "/opt/homebrew/bin/podman",
                "/usr/local/bin/podman",
                "/usr/bin/podman",
            ]
            .map(PathBuf::from),
        )
        .find(|p| p.is_file())
}

/// The default machine in `podman machine list --format json`, else the
/// first one listed. A valid empty array confirms there is no machine.
fn machine_state_from(list: &serde_json::Value) -> Result<&'static str, String> {
    let machines = list.as_array().ok_or_else(|| {
        "Podman returned invalid machine-list JSON (expected an array).".to_string()
    })?;
    let Some(m) = machines
        .iter()
        .find(|m| m["Default"].as_bool() == Some(true))
        .or_else(|| machines.first())
    else {
        return Ok("missing");
    };
    if m["Running"].as_bool() == Some(true) {
        Ok("running")
    } else if m["Starting"].as_bool() == Some(true) {
        Ok("starting")
    } else if m["Running"].as_bool() == Some(false) {
        Ok("stopped")
    } else {
        Err("Podman returned machine data without a valid Running state.".into())
    }
}

async fn machine_state(podman: &Path) -> Result<&'static str, String> {
    let out = tokio::process::Command::new(podman)
        .args(["machine", "list", "--format", "json"])
        .output()
        .await
        .map_err(|e| format!("Could not run `podman machine list`: {e}"))?;
    if !out.status.success() {
        let detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if detail.is_empty() {
            format!("`podman machine list` exited with {}.", out.status)
        } else {
            format!("`podman machine list` failed: {detail}")
        });
    }
    let list: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("Podman returned invalid machine-list JSON: {e}"))?;
    machine_state_from(&list)
}

fn machine_failure(status: &SetupStatus) -> Option<String> {
    if status.platform != "macos" {
        return None;
    }
    match status.machine {
        Some("running" | "starting" | "stopped") => None,
        Some("missing") => Some("No Podman machine exists. Create one with `podman machine init`.".into()),
        Some("error") | None => Some(status.machine_error.clone().unwrap_or_else(|| {
            "Could not verify the Podman machine. Retry detection before installing the service.".into()
        })),
        _ => Some("Could not verify the Podman machine. Retry detection before installing the service.".into()),
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
    let podman = find_podman();
    let (machine, machine_error) = match &podman {
        Some(p) if cfg!(target_os = "macos") => match machine_state(p).await {
            Ok(state) => (Some(state), None),
            Err(error) => (Some("error"), Some(error)),
        },
        _ => (None, None),
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
        machine_error,
        machine,
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
    if initial.podman.is_none() {
        return Err(
            "Podman is required before installing the service. Install Podman, then retry setup."
                .into(),
        );
    }
    if let Some(reason) = machine_failure(&initial) {
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
    use serde_json::json;

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
    fn the_machine_state_is_the_default_machines() {
        let one = |running: bool, starting: bool| json!([{"Name": "m", "Default": true, "Running": running, "Starting": starting}]);
        assert_eq!(machine_state_from(&one(true, false)), Ok("running"));
        assert_eq!(machine_state_from(&one(false, true)), Ok("starting"));
        assert_eq!(machine_state_from(&one(false, false)), Ok("stopped"));
        assert_eq!(machine_state_from(&json!([])), Ok("missing"));
        let two = json!([
            {"Name": "other", "Default": false, "Running": true},
            {"Name": "main", "Default": true, "Running": false}
        ]);
        assert_eq!(machine_state_from(&two), Ok("stopped"));
        assert!(machine_state_from(&json!({"machines": []})).is_err());
        assert!(machine_state_from(&json!([{"Name": "main"}])).is_err());
    }

    #[test]
    fn service_install_requires_a_verified_machine_on_macos() {
        let mut status = SetupStatus {
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
            podman: Some("/usr/bin/podman".into()),
            machine: Some("error"),
            machine_error: Some("probe failed".into()),
        };
        assert_eq!(machine_failure(&status).as_deref(), Some("probe failed"));
        status.machine = Some("missing");
        assert!(machine_failure(&status)
            .unwrap()
            .contains("podman machine init"));
        status.machine = Some("stopped");
        assert_eq!(machine_failure(&status), None);
    }
}
