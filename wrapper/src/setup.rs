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

pub fn owner(answering: bool, migrated: bool, service_running: bool) -> Owner {
    match (answering, migrated, service_running) {
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
    pub podman: Option<String>,
    /// macOS only: `running`, `starting`, `stopped` or `missing`.
    pub machine: Option<&'static str>,
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
/// first one listed.
fn machine_state_from(list: &serde_json::Value) -> &'static str {
    let machines = list.as_array().map(Vec::as_slice).unwrap_or_default();
    let Some(m) = machines
        .iter()
        .find(|m| m["Default"].as_bool() == Some(true))
        .or_else(|| machines.first())
    else {
        return "missing";
    };
    if m["Running"].as_bool() == Some(true) {
        "running"
    } else if m["Starting"].as_bool() == Some(true) {
        "starting"
    } else {
        "stopped"
    }
}

async fn machine_state(podman: &Path) -> &'static str {
    let out = tokio::process::Command::new(podman)
        .args(["machine", "list", "--format", "json"])
        .output()
        .await;
    out.ok()
        .and_then(|o| serde_json::from_slice(&o.stdout).ok())
        .map(|v| machine_state_from(&v))
        .unwrap_or("missing")
}

pub async fn status(http: &reqwest::Client, url: &str) -> SetupStatus {
    let answering = node::answering(http, url).await;
    let node_version = if answering {
        node::running_version(http, url).await
    } else {
        None
    };
    let migrated = node::migrated_node(&node::state_dir()).is_some();
    let service_running = service::running();
    let cli = node::installed_path();
    let path_hint = match cli.as_deref().and_then(Path::parent) {
        Some(dir) => node::path_hint(dir).await,
        None => None,
    };
    let podman = find_podman();
    let machine = match &podman {
        Some(p) if cfg!(target_os = "macos") => Some(machine_state(p).await),
        _ => None,
    };
    SetupStatus {
        platform: if cfg!(target_os = "macos") {
            "macos"
        } else {
            "linux"
        },
        owner: owner(answering, migrated, service_running),
        node_version,
        sidecar_version: node::sidecar_path().and_then(|p| node::version_of(&p)),
        cli_version: cli.as_deref().and_then(node::version_of),
        cli_path: cli.map(|p| p.display().to_string()),
        path_hint,
        service_installed: service::installed(),
        service_running,
        podman: podman.map(|p| p.display().to_string()),
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
    match status(http, url).await.owner {
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
    blocking(move || service::install(&cli)).await?;
    if !node::wait_ready(http, url).await {
        return Err(
            "the service started, but the node did not answer within a minute; `tracon service status` says why"
                .into(),
        );
    }
    Ok(status(http, url).await)
}

pub async fn install_cli(http: &reqwest::Client, url: &str) -> Result<SetupStatus, String> {
    blocking(node::install_cli).await?;
    Ok(status(http, url).await)
}

pub async fn restart_node(http: &reqwest::Client, url: &str) -> Result<SetupStatus, String> {
    let cli = node::installed_path().ok_or("HOME is not set")?;
    blocking(move || service::restart(&cli)).await?;
    if !node::wait_ready(http, url).await {
        return Err("the node did not answer within a minute of restarting".into());
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
    }

    #[test]
    fn the_machine_state_is_the_default_machines() {
        let one = |running: bool, starting: bool| json!([{"Name": "m", "Default": true, "Running": running, "Starting": starting}]);
        assert_eq!(machine_state_from(&one(true, false)), "running");
        assert_eq!(machine_state_from(&one(false, true)), "starting");
        assert_eq!(machine_state_from(&one(false, false)), "stopped");
        assert_eq!(machine_state_from(&json!([])), "missing");
        let two = json!([
            {"Name": "other", "Default": false, "Running": true},
            {"Name": "main", "Default": true, "Running": false}
        ]);
        assert_eq!(machine_state_from(&two), "stopped");
    }
}
