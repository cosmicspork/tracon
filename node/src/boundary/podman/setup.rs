//! `tracon setup`: bring in what every sandbox starts from — the harness
//! bundle for this release and the pinned stock base, and the browser bundle
//! where a service asks for it — and remove the gateway an older node made.
//! Idempotent: running it again reconciles rather than failing. The node runs
//! the same steps itself (`converge`, through `boundary::reconcile`).

use crate::config::Config;

use super::podman;
use crate::boundary::BoundaryError;

/// Each harness's image here, which is the stock base every harness runs in
/// from the bundle, and whether it is present.
pub async fn harness_images(cfg: &Config) -> Vec<crate::boundary::HarnessImage> {
    let image = crate::sandbox_images::base_image(cfg);
    let present = podman(&["image", "exists", &image]).await.is_ok();
    crate::adapter::enabled(cfg)
        .into_iter()
        .map(|id| crate::boundary::HarnessImage {
            harness_id: id.to_string(),
            image: image.clone(),
            state: if present { "current" } else { "missing" },
        })
        .collect()
}

fn label_value(out: &str) -> Option<String> {
    let v = out.trim();
    (!v.is_empty() && v != "<no value>").then(|| v.to_string())
}

/// `tracon setup`: everything converged. `rebuild` asks for nothing more:
/// there are no images of the node's own to build.
pub async fn setup(cfg: &Config, _rebuild: bool) -> Result<(), BoundaryError> {
    converge(cfg).await.map(|_| ())
}

/// The node's own setup: only what is out of step changes. `Ok(true)` when
/// anything changed.
pub async fn converge(cfg: &Config) -> Result<bool, BoundaryError> {
    let fetched = crate::sandbox_images::converge(cfg)
        .await
        .map_err(BoundaryError::Other)?;
    std::fs::create_dir_all(Config::harness_state_dir())?;
    let retired = retire_gateway(&SystemHost, &retired_gateway(cfg)).await?;
    // Last: a browser that cannot be fetched leaves sessions the rest.
    let browser = crate::service_bundle::converge(cfg)
        .await
        .map_err(BoundaryError::Other)?;
    Ok(fetched || retired || browser)
}

/// A digest of everything [`converge`] fetches, cheap enough to compare every
/// few minutes.
pub fn definitions(cfg: &Config) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(
        crate::sandbox_images::definitions(cfg).as_bytes(),
    ))
}

/// The user service an older node ran its gateway under, on a Linux host.
/// Kept by name so an upgraded node removes it, and `tracon service
/// uninstall` with it.
pub const GATEWAY_UNIT: &str = "tracon-gateway.service";

/// The default gateway container, whose unit is [`GATEWAY_UNIT`].
const DEFAULT_GATEWAY: &str = "tracon-gw";

/// The unit an older node ran this node's gateway under: a second node on
/// the machine (a scratch node with its own `gateway_container`) had its own.
pub fn gateway_unit(cfg: &Config) -> String {
    let name = cfg.boundary.gateway_container.as_str();
    if name == DEFAULT_GATEWAY || name.is_empty() {
        return GATEWAY_UNIT.into();
    }
    let safe: String = name
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '_',
        })
        .collect();
    format!("tracon-gateway-{safe}.service")
}

/// The label a gateway container carried, which is how retiring one tells a
/// gateway the node made from a container of the same name it did not.
const GATEWAY_LABEL: &str = "io.tracon.gateway";

/// The gateway objects an older node made, which this one removes: its unit,
/// container and internal network, as its configuration named them. `None`
/// where the name is the default node's and this node has a prefix of its
/// own: a scratch node left on the default names must not take down the
/// gateway of the node beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetiredGateway {
    pub unit: Option<String>,
    pub container: Option<String>,
    pub network: Option<String>,
}

pub fn retired_gateway(cfg: &Config) -> RetiredGateway {
    let own_prefix = cfg.boundary.resource_prefix != crate::config::DEFAULT_RESOURCE_PREFIX;
    let ours = |name: String, default: &str| {
        (!name.is_empty() && !(own_prefix && name == default)).then_some(name)
    };
    RetiredGateway {
        unit: ours(gateway_unit(cfg), GATEWAY_UNIT),
        container: ours(cfg.boundary.gateway_container.clone(), DEFAULT_GATEWAY),
        network: ours(
            cfg.retired.network.clone(),
            crate::config::DEFAULT_GATEWAY_NETWORK,
        ),
    }
}

/// What retiring a gateway runs on: podman, the user manager, and where a
/// unit's file is.
#[async_trait::async_trait]
pub(crate) trait GatewayHost: Send + Sync {
    async fn podman(&self, args: &[&str]) -> Result<String, BoundaryError>;
    async fn systemctl(&self, args: &[&str]) -> Result<(), BoundaryError>;
    fn unit_file(&self, unit: &str) -> Option<std::path::PathBuf>;
}

struct SystemHost;

#[async_trait::async_trait]
impl GatewayHost for SystemHost {
    async fn podman(&self, args: &[&str]) -> Result<String, BoundaryError> {
        podman(args).await
    }
    async fn systemctl(&self, args: &[&str]) -> Result<(), BoundaryError> {
        systemctl(args).await
    }
    fn unit_file(&self, unit: &str) -> Option<std::path::PathBuf> {
        unit_path(unit)
    }
}

/// Stop, disable and remove the gateway's unit, then remove its container and
/// network, whichever of them are still there. `Ok(true)` when it removed
/// anything, which is logged once: the next converge finds nothing left to
/// remove.
pub(crate) async fn retire_gateway(
    host: &dyn GatewayHost,
    gateway: &RetiredGateway,
) -> Result<bool, BoundaryError> {
    let mut removed: Vec<String> = Vec::new();
    if let Some(unit) = &gateway.unit {
        if let Some(path) = host.unit_file(unit).filter(|path| path.exists()) {
            // Stopping runs the unit's `ExecStop`, which removes its
            // container. A host whose user manager is gone still loses the
            // file, so nothing starts it at the next login.
            if let Err(error) = host.systemctl(&["disable", "--now", unit]).await {
                tracing::warn!(%unit, %error, "could not stop the gateway's unit");
            }
            std::fs::remove_file(&path)?;
            host.systemctl(&["daemon-reload"]).await?;
            removed.push(unit.clone());
        }
    }
    if let Some(container) = &gateway.container {
        // Only a container the node made as its gateway, whatever the
        // configuration calls it.
        let format = format!("{{{{ index .Config.Labels \"{GATEWAY_LABEL}\" }}}}");
        let made = host
            .podman(&["container", "inspect", "--format", &format, container])
            .await
            .ok()
            .and_then(|out| label_value(&out));
        if made.is_some() {
            host.podman(&["rm", "-f", "-i", container]).await?;
            removed.push(container.clone());
        }
    }
    if let Some(network) = &gateway.network {
        if host.podman(&["network", "exists", network]).await.is_ok() {
            // Never forced: that would remove whatever is still on it.
            match host.podman(&["network", "rm", network]).await {
                Ok(_) => removed.push(network.clone()),
                Err(error) => tracing::warn!(
                    %network,
                    %error,
                    "left the gateway's network, which something is still on; the next setup removes it"
                ),
            }
        }
    }
    if !removed.is_empty() {
        tracing::info!(
            removed = %removed.join(", "),
            "sandboxes reach the node through tunnels; removed the gateway an older node made"
        );
    }
    Ok(!removed.is_empty())
}

fn unit_path(unit: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|home| {
        std::path::PathBuf::from(home)
            .join(".config/systemd/user")
            .join(unit)
    })
}

async fn systemctl(args: &[&str]) -> Result<(), BoundaryError> {
    let out = tokio::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .await?;
    if out.status.success() {
        Ok(())
    } else {
        Err(BoundaryError::Other(format!(
            "systemctl --user {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A host holding some of a gateway: a unit file in a directory of the
    /// test's own, containers that carry the gateway's label, and networks.
    struct FakeHost {
        units: tempfile::TempDir,
        containers: Mutex<Vec<String>>,
        networks: Mutex<Vec<String>>,
        /// Networks something is still on.
        busy: Vec<String>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeHost {
        fn new(unit: Option<&str>, containers: &[&str], networks: &[&str]) -> FakeHost {
            let units = tempfile::tempdir().unwrap();
            if let Some(unit) = unit {
                std::fs::write(units.path().join(unit), "[Service]\n").unwrap();
            }
            FakeHost {
                units,
                containers: Mutex::new(containers.iter().map(|c| c.to_string()).collect()),
                networks: Mutex::new(networks.iter().map(|n| n.to_string()).collect()),
                busy: Vec::new(),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl GatewayHost for FakeHost {
        async fn podman(&self, args: &[&str]) -> Result<String, BoundaryError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("podman {}", args.join(" ")));
            let missing = || Err(BoundaryError::Podman("no such object".into()));
            match args {
                ["container", "inspect", .., name] => {
                    match self.containers.lock().unwrap().iter().any(|c| c == name) {
                        true => Ok("0123abcd\n".into()),
                        false => missing(),
                    }
                }
                ["rm", "-f", "-i", name] => {
                    self.containers.lock().unwrap().retain(|c| c != name);
                    Ok(String::new())
                }
                ["network", "exists", name] => {
                    match self.networks.lock().unwrap().iter().any(|n| n == name) {
                        true => Ok(String::new()),
                        false => missing(),
                    }
                }
                ["network", "rm", name] if self.busy.iter().any(|n| n == name) => Err(
                    BoundaryError::Podman(format!("network {name} is being used")),
                ),
                ["network", "rm", name] => {
                    self.networks.lock().unwrap().retain(|n| n != name);
                    Ok(String::new())
                }
                other => panic!("unexpected podman {other:?}"),
            }
        }
        async fn systemctl(&self, args: &[&str]) -> Result<(), BoundaryError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("systemctl {}", args.join(" ")));
            // Stopping the unit runs its `ExecStop`, which removes the
            // container it ran.
            if args.first() == Some(&"disable") {
                self.containers.lock().unwrap().clear();
            }
            Ok(())
        }
        fn unit_file(&self, unit: &str) -> Option<std::path::PathBuf> {
            Some(self.units.path().join(unit))
        }
    }

    fn named(prefix: &str, gateway: &str, network: &str) -> Config {
        let text = format!(
            "[boundary]\nresource_prefix = \"{prefix}\"\ngateway_container = \"{gateway}\"\n\
             network = \"{network}\"\n"
        );
        Config::from_toml(&text, std::path::Path::new("node.toml")).unwrap()
    }

    #[test]
    fn a_converge_retires_the_gateway_by_the_names_the_node_gave_it() {
        assert_eq!(
            retired_gateway(&Config::default()),
            RetiredGateway {
                unit: Some(GATEWAY_UNIT.into()),
                container: Some("tracon-gw".into()),
                network: Some("tracon-int".into()),
            }
        );
        // A second node retires its own, by the names it gave them.
        assert_eq!(
            retired_gateway(&named("spike-a4", "spike-a4-gw", "spike-a4-int")),
            RetiredGateway {
                unit: Some("tracon-gateway-spike-a4-gw.service".into()),
                container: Some("spike-a4-gw".into()),
                network: Some("spike-a4-int".into()),
            }
        );
        // One that kept the default names never takes the default node's.
        assert_eq!(
            retired_gateway(&named("spike-a4", "tracon-gw", "tracon-int")),
            RetiredGateway {
                unit: None,
                container: None,
                network: None,
            }
        );
    }

    #[tokio::test]
    async fn retiring_the_gateway_stops_its_unit_and_removes_what_is_left_once() {
        let unit = "tracon-gateway-spike-a4-gw.service";
        let host = FakeHost::new(Some(unit), &["spike-a4-gw"], &["spike-a4-int"]);
        let gateway = RetiredGateway {
            unit: Some(unit.into()),
            container: Some("spike-a4-gw".into()),
            network: Some("spike-a4-int".into()),
        };
        assert!(retire_gateway(&host, &gateway).await.unwrap());
        assert!(!host.units.path().join(unit).exists(), "unit file removed");
        assert!(host.containers.lock().unwrap().is_empty());
        assert!(host.networks.lock().unwrap().is_empty());
        let calls = host.calls();
        assert_eq!(calls[0], format!("systemctl disable --now {unit}"));
        assert_eq!(calls[1], "systemctl daemon-reload");
        assert!(calls.contains(&"podman network rm spike-a4-int".to_string()));

        // Nothing left: nothing changes, nothing is said again.
        host.calls.lock().unwrap().clear();
        assert!(!retire_gateway(&host, &gateway).await.unwrap());
        assert!(host
            .calls()
            .iter()
            .all(|c| !c.contains(" rm ") && !c.starts_with("systemctl")));
    }

    #[tokio::test]
    async fn a_bare_gateway_goes_and_a_network_still_in_use_stays() {
        // An older setup's gateway, a plain container with no unit, and a
        // session from before still on the network.
        let mut host = FakeHost::new(None, &["tracon-gw"], &["tracon-int"]);
        host.busy = vec!["tracon-int".into()];
        let gateway = RetiredGateway {
            unit: Some(GATEWAY_UNIT.into()),
            container: Some("tracon-gw".into()),
            network: Some("tracon-int".into()),
        };
        assert!(retire_gateway(&host, &gateway).await.unwrap());
        assert!(host.containers.lock().unwrap().is_empty());
        assert_eq!(host.networks.lock().unwrap().clone(), ["tracon-int"]);
        assert!(!host.calls().iter().any(|c| c.starts_with("systemctl")));
        assert!(!host.calls().iter().any(|c| c.contains("network rm -f")));

        // Names it was told to leave are never asked about.
        let host = FakeHost::new(Some(GATEWAY_UNIT), &["tracon-gw"], &["tracon-int"]);
        let left = RetiredGateway {
            unit: None,
            container: None,
            network: None,
        };
        assert!(!retire_gateway(&host, &left).await.unwrap());
        assert!(host.calls().is_empty());
        assert!(host.units.path().join(GATEWAY_UNIT).exists());
    }

    #[test]
    fn a_second_gateway_gets_a_unit_of_its_own() {
        let mut cfg = Config::default();
        assert_eq!(gateway_unit(&cfg), GATEWAY_UNIT);
        cfg.boundary.gateway_container = "spike-u2-gw".into();
        assert_eq!(gateway_unit(&cfg), "tracon-gateway-spike-u2-gw.service");
        cfg.boundary.gateway_container = "odd name/x".into();
        assert_eq!(gateway_unit(&cfg), "tracon-gateway-odd_name_x.service");
    }

    #[test]
    fn allowlist_entries_are_anchored_exactly_once() {
        use crate::gateway::proxy::anchor;
        // A plain host becomes an exact match, so a suffix cannot slip past.
        assert_eq!(anchor("api.openai.com"), "^api.openai.com$");
        // An already-anchored regex is left alone.
        assert_eq!(anchor("^api\\.openai\\.com$"), "^api\\.openai\\.com$");
        // Half-anchored entries get only the missing end.
        assert_eq!(anchor("^api.openai.com"), "^api.openai.com$");
        assert_eq!(anchor("api.openai.com$"), "^api.openai.com$");
    }
}
