//! `tracon setup`: create the network, allowlist, and gateway the node owns.
//! Idempotent — running it again reconciles rather than failing.

use crate::config::Config;

use super::{podman, BoundaryError};

pub async fn setup(cfg: &Config) -> Result<(), BoundaryError> {
    ensure_network(cfg).await?;
    write_allowlist(cfg)?;
    ensure_gateway(cfg).await?;
    std::fs::create_dir_all(Config::harness_state_dir())?;
    Ok(())
}

async fn ensure_network(cfg: &Config) -> Result<(), BoundaryError> {
    if podman(&["network", "exists", &cfg.boundary.network])
        .await
        .is_ok()
    {
        tracing::info!(network = %cfg.boundary.network, "network exists");
        return Ok(());
    }
    // `--internal` removes the route out; `--disable-dns` stops the network's
    // resolver answering for every external name.
    podman(&[
        "network",
        "create",
        "--internal",
        "--disable-dns",
        "--subnet",
        &cfg.boundary.subnet,
        &cfg.boundary.network,
    ])
    .await?;
    tracing::info!(network = %cfg.boundary.network, "network created");
    Ok(())
}

fn write_allowlist(cfg: &Config) -> Result<(), BoundaryError> {
    let path = Config::allow_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = cfg
        .gateway
        .allow_hosts
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, format!("{body}\n"))?;
    tracing::info!(path = %path.display(), hosts = cfg.gateway.allow_hosts.len(), "allowlist written");
    Ok(())
}

async fn ensure_gateway(cfg: &Config) -> Result<(), BoundaryError> {
    let _ = podman(&["rm", "-f", "-i", &cfg.boundary.gateway_container]).await;
    let allow = Config::allow_file();
    let mount = format!("{}:/etc/tinyproxy/allow.txt:ro", allow.display());
    let net_int = format!("{}:ip={}", cfg.boundary.network, cfg.boundary.gateway_ip);
    // On a Podman machine the node is outside the VM, so the gateway forwards to
    // the host's loopback listener. A Linux node uses its own Unix socket.
    let upstream = format!("TCP:host.containers.internal:{}", cfg.gateway.forward_port);
    let upstream_env = format!("TRACON_UPSTREAM={upstream}");
    let listen_env = format!("TRACON_LISTEN_IP={}", cfg.boundary.gateway_ip);
    podman(&[
        "run",
        "-d",
        "--name",
        &cfg.boundary.gateway_container,
        // The default network gives the gateway its own egress; the internal
        // one is how the harness reaches it.
        "--network",
        "podman",
        "--network",
        &net_int,
        "-v",
        &mount,
        "-e",
        &upstream_env,
        "-e",
        &listen_env,
        &cfg.boundary.gateway_image,
    ])
    .await?;
    tracing::info!(container = %cfg.boundary.gateway_container, "gateway started");
    Ok(())
}
