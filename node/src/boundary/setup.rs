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
    // tinyproxy matches these as unanchored regexes, so `api.openai.com` would
    // also match `api.openai.com.evil.com`. Anchor every entry that is not
    // already anchored, so a hand-added host is an exact match rather than a
    // substring one. An operator who wants a pattern can still write `.*`.
    let body = cfg
        .gateway
        .allow_hosts
        .iter()
        .map(|h| anchor(h))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, format!("{body}\n"))?;
    tracing::info!(path = %path.display(), hosts = cfg.gateway.allow_hosts.len(), "allowlist written");
    Ok(())
}

/// Anchor a tinyproxy allowlist entry at both ends so it matches a whole host,
/// not a substring. Already-anchored entries are left as they are.
fn anchor(host: &str) -> String {
    let mut s = host.to_string();
    if !s.starts_with('^') {
        s.insert(0, '^');
    }
    if !s.ends_with('$') {
        s.push('$');
    }
    s
}

async fn ensure_gateway(cfg: &Config) -> Result<(), BoundaryError> {
    let _ = podman(&["rm", "-f", "-i", &cfg.boundary.gateway_container]).await;
    let allow = Config::allow_file();
    let mount = format!("{}:/etc/tinyproxy/allow.txt:ro", allow.display());
    let net_int = format!("{}:ip={}", cfg.boundary.network, cfg.boundary.gateway_ip);
    // On a Podman machine the node is outside the VM, so the gateway forwards to
    // the host's loopback listener over TCP via `host.containers.internal`.
    //
    // TODO(linux-node): the docs describe a Linux node forwarding over a Unix
    // socket under $XDG_RUNTIME_DIR mounted into the gateway, which is not yet
    // implemented — this path is TCP on every platform. macOS (the first node)
    // is unaffected; a Linux node needs the Unix-socket forward before its node
    // listener can stay on loopback. Tracked for when a Linux node is stood up.
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

#[cfg(test)]
mod tests {
    use super::anchor;

    #[test]
    fn allowlist_entries_are_anchored_exactly_once() {
        // A plain host becomes an exact match, so a suffix cannot slip past.
        assert_eq!(anchor("api.openai.com"), "^api.openai.com$");
        // An already-anchored regex is left alone.
        assert_eq!(anchor("^api\\.openai\\.com$"), "^api\\.openai\\.com$");
        // Half-anchored entries get only the missing end.
        assert_eq!(anchor("^api.openai.com"), "^api.openai.com$");
        assert_eq!(anchor("api.openai.com$"), "^api.openai.com$");
    }
}
