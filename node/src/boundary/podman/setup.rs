//! `tracon setup`: create the network, allowlist, and gateway the node owns.
//! Idempotent — running it again reconciles rather than failing.

use rust_embed::Embed;

use crate::config::{Config, HarnessListen};

use super::podman;
use crate::boundary::BoundaryError;

/// The gateway and harness definitions, carried inside the binary so a host
/// that only fetched the release can build the images it needs.
#[derive(Embed)]
#[folder = "../containers"]
struct Containers;

/// The label a built image carries: a digest of the definitions it was built
/// from, so a node on a newer binary can tell its images are stale.
pub const DEFINITIONS_LABEL: &str = "io.tracon.definitions";

/// The directory the harness image is built from. The claude harness has its
/// own definitions; the omp ones under its tag would run the wrong CLI.
pub fn harness_dir(cfg: &Config) -> &'static str {
    if cfg.harness.id == "claude" {
        "harness-claude"
    } else {
        "harness"
    }
}

/// Each image the node runs, with the directory it is built from.
pub fn images(cfg: &Config) -> [(&str, &'static str); 2] {
    [
        (cfg.boundary.gateway_image.as_str(), "gateway"),
        (cfg.boundary.harness_image.as_str(), harness_dir(cfg)),
    ]
}

/// A digest of one image's embedded definitions: every file under `dir`, in
/// path order.
pub fn definitions_digest(dir: &str) -> String {
    use sha2::{Digest, Sha256};
    let prefix = format!("{dir}/");
    let mut paths: Vec<_> = Containers::iter()
        .filter(|p| p.starts_with(&prefix))
        .collect();
    paths.sort();
    let mut hash = Sha256::new();
    for path in paths {
        if let Some(file) = Containers::get(&path) {
            hash.update(path.as_bytes());
            hash.update([0]);
            hash.update(file.data.as_ref());
            hash.update([0]);
        }
    }
    hex::encode(hash.finalize())
}

/// The definitions digest an image was built with, if it carries one. Images
/// built before the label existed, or by hand, carry none.
pub async fn image_digest(image: &str) -> Option<String> {
    let format = format!("{{{{ index .Labels \"{DEFINITIONS_LABEL}\" }}}}");
    podman(&["image", "inspect", "--format", &format, image])
        .await
        .ok()
        .and_then(|out| label_value(&out))
}

fn label_value(out: &str) -> Option<String> {
    let v = out.trim();
    (!v.is_empty() && v != "<no value>").then(|| v.to_string())
}

pub async fn setup(cfg: &Config, rebuild: bool) -> Result<(), BoundaryError> {
    super::ensure_machine(cfg).await?;
    ensure_images(cfg, rebuild).await?;
    ensure_network(cfg).await?;
    write_allowlist(cfg)?;
    ensure_gateway(cfg).await?;
    std::fs::create_dir_all(Config::harness_state_dir())?;
    Ok(())
}

/// Write the embedded container definitions out and build any image that is
/// missing or was built from definitions other than this binary's. The
/// harness image fetches the pinned harness release at build time, the one
/// network fetch a fresh node makes.
async fn ensure_images(cfg: &Config, rebuild: bool) -> Result<(), BoundaryError> {
    let root = Config::containers_dir();
    for path in Containers::iter() {
        let Some(file) = Containers::get(&path) else {
            continue;
        };
        let dest = root.join(path.as_ref());
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&dest, file.data.as_ref())?;
    }
    for (image, dir) in images(cfg) {
        let digest = definitions_digest(dir);
        let current = podman(&["image", "exists", image]).await.is_ok()
            && image_digest(image).await.as_deref() == Some(digest.as_str());
        if current && !rebuild {
            tracing::info!(image, "image current");
            continue;
        }
        let ctx = root.join(dir);
        let label = format!("{DEFINITIONS_LABEL}={digest}");
        tracing::info!(image, context = %ctx.display(), "building image");
        podman(&[
            "build",
            "--label",
            &label,
            "-t",
            image,
            &ctx.to_string_lossy(),
        ])
        .await?;
    }
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
pub fn anchor(host: &str) -> String {
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
    let net_int = format!("{}:ip={}", cfg.boundary.network, cfg.boundary.gateway_ip);
    // Two forwards. On a Podman machine the node is outside the VM and gvproxy
    // reaches the host's loopback, so TCP via `host.containers.internal` works.
    // On a Linux host that name is a pasta interface address, not loopback, so
    // the node listens on a Unix socket and the gateway mounts its directory.
    let selinux = super::selinux_enabled().await;
    // Under SELinux a plain bind mount is unreadable from the container (the
    // gateway died on "allow.txt missing" on an SELinux host); `:z` relabels the node's
    // own files, which is fine for state tracon owns.
    let mount = format!(
        "{}:/etc/tinyproxy/allow.txt:ro{}",
        allow.display(),
        if selinux { ",z" } else { "" }
    );
    let (upstream, socket_mount) = match &cfg.gateway.harness_listen {
        HarnessListen::Tcp(addr) => (
            format!("TCP:host.containers.internal:{}", addr.port()),
            None,
        ),
        HarnessListen::Unix(path) => {
            let dir = path
                .parent()
                .ok_or_else(|| BoundaryError::Podman("harness socket path has no parent".into()))?;
            std::fs::create_dir_all(dir)?;
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "harness.sock".into());
            let label = if selinux { ":z" } else { "" };
            (
                format!("UNIX-CONNECT:/run/tracon/{name}"),
                Some(format!("{}:/run/tracon{label}", dir.display())),
            )
        }
    };
    let upstream_env = format!("TRACON_UPSTREAM={upstream}");
    let listen_env = format!("TRACON_LISTEN_IP={}", cfg.boundary.gateway_ip);
    let mut args: Vec<&str> = vec![
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
    ];
    if let Some(m) = &socket_mount {
        args.push("-v");
        args.push(m);
        // SELinux forbids a confined container process from connecting to a
        // socket whose listener is unconfined (`connectto`), whatever the file
        // is labelled. The gateway is the trusted, node-owned piece — it exists
        // so the harness never touches the socket — so it runs unconfined; the
        // harness keeps its label.
        if selinux {
            args.push("--security-opt");
            args.push("label=disable");
        }
    }
    args.extend([
        "-e",
        &upstream_env,
        "-e",
        &listen_env,
        &cfg.boundary.gateway_image,
    ]);
    podman(&args).await?;
    tracing::info!(container = %cfg.boundary.gateway_container, "gateway started");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_image_has_its_own_definitions_digest() {
        let gateway = definitions_digest("gateway");
        assert_eq!(gateway, definitions_digest("gateway"));
        assert_eq!(gateway.len(), 64);
        assert_ne!(gateway, definitions_digest("harness"));
        assert_ne!(
            definitions_digest("harness"),
            definitions_digest("harness-claude")
        );
        assert_ne!(gateway, definitions_digest("nonexistent"));
    }

    #[test]
    fn the_claude_harness_is_built_from_its_own_definitions() {
        let mut cfg = Config::default();
        assert_eq!(harness_dir(&cfg), "harness");
        cfg.harness.id = "claude".into();
        assert_eq!(harness_dir(&cfg), "harness-claude");
    }

    #[test]
    fn an_unlabelled_image_has_no_digest() {
        assert_eq!(label_value("<no value>\n"), None);
        assert_eq!(label_value(""), None);
        assert_eq!(label_value("abc123\n"), Some("abc123".into()));
    }

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
