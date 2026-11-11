//! `tracon sandbox probe`: make one sandbox with a provider, connect to it
//! over the sandbox protocol, run a command in it, read its isolation back
//! from the provider, and remove it. A check that a provider works on this
//! host, end to end, before any session depends on it.
//!
//! On Kubernetes it runs inside the node's pod (`kubectl exec`), with the
//! node's own pod facts, client and `[runtime.kubernetes]` settings: one
//! sandbox pod beside the node's, which dials this process on the sandbox
//! port and fetches the bundle there, held to the sandbox NetworkPolicy the
//! deployment carries, which `verify_isolation` reads back. It is for a node
//! about to turn `sandbox_protocol` on: a node already serving its sandboxes
//! holds the port, and its boundary check covers the same ground.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use tracon_sandbox::node::{SandboxServer, ServerOptions};
use tracon_sandbox::protocol::SpawnSpec;
use tracon_sandbox::tls::NodeIdentity;

use super::apple::{AppleOptions, AppleProvider, BUNDLE_TARGET};
use super::capabilities::{Dns, Route};
use super::kubernetes::{KubeSandboxOptions, KubeSandboxProvider};
use super::podman::PodmanProvider;
use super::{
    Bootstrap, Capabilities, CreateSpec, Handle, ProviderKind, Resources, SandboxProvider,
};

/// How long the sandbox has to start and take the node's connection.
const CONNECT_WITHIN: Duration = Duration::from_secs(60);

pub struct ProbeOptions {
    pub provider: ProviderKind,
    /// The image to run; `None` is the provider's default ([`default_image`]).
    pub image: Option<String>,
    /// The harness bundle for the provider's architecture, holding
    /// `bin/tracon-sandbox`. `None` is this release's for this host, fetched
    /// and checked as the node fetches its own.
    pub bundle: Option<PathBuf>,
    /// Leave the sandbox running afterwards, for a look inside.
    pub keep: bool,
    /// What to call the sandbox, where the provider names it as asked.
    pub name: Option<String>,
}

/// The image a probe runs when it is given none: any small image does where
/// the bundle is mounted from the host; on Kubernetes the sandbox's own image
/// fetches the bundle (a shell, `curl` or `wget`, `sha256sum` and `tar`), so
/// it is the stock base the node's sandboxes run.
pub fn default_image(provider: ProviderKind, cfg: &crate::config::Config) -> String {
    match provider {
        ProviderKind::Kubernetes => cfg.sandbox.base_image_ref(),
        _ => "ubuntu:24.04".into(),
    }
}

/// What one probe saw. Each line is printed as it happens.
pub async fn run(options: ProbeOptions) -> Result<()> {
    let cfg = crate::config::Config::load();
    let bundle = match &options.bundle {
        Some(bundle) => bundle.clone(),
        None => {
            let ensured = crate::bundle::ensure_release(&cfg)
                .await
                .context("fetch this release's harness bundle (or pass --bundle)")?;
            line("bundle", ensured.path().display().to_string());
            ensured.path().to_path_buf()
        }
    };
    let bundle = bundle
        .canonicalize()
        .with_context(|| format!("bundle {}", bundle.display()))?;
    let image = options
        .image
        .clone()
        .unwrap_or_else(|| default_image(options.provider, &cfg));
    let provider: Box<dyn SandboxProvider> = match options.provider {
        ProviderKind::Apple => Box::new(AppleProvider::new(AppleOptions {
            bundle: bundle.clone(),
            socket_dir: AppleOptions::default_socket_dir(),
            limit: None,
            ..AppleOptions::default()
        })),
        // The boundary this node's configuration describes: its network,
        // gateway and proxy, as a session's sandbox gets them.
        ProviderKind::Podman => {
            let mut cfg = cfg.clone();
            cfg.sandbox.bundle = Some(bundle.clone());
            let spec = crate::runner::podman::RunSpec::from_config(&cfg, false);
            Box::new(PodmanProvider::from_config(&cfg, spec))
        }
        ProviderKind::Kubernetes => Box::new(kubernetes(&cfg, &bundle).await?),
        other => bail!("the probe does not support {} yet", other.as_str()),
    };
    let supervisor = bundle.join("bin/tracon-sandbox");
    if !supervisor.is_file() {
        bail!(
            "{} is missing: build tracon-sandbox for the provider's architecture into the bundle",
            supervisor.display()
        );
    }

    // A throwaway identity: the probe is its own node for one run.
    let identity = NodeIdentity::generate()?;
    let server = SandboxServer::new(&identity, ServerOptions::default())?;
    let bootstrap = Bootstrap {
        run: "probe".into(),
        cert_pin: server.pin().to_string(),
        token: server.tokens().issue("probe"),
    };

    let started = Instant::now();
    let handle = provider
        .create(&CreateSpec {
            name: options.name.as_deref(),
            image: &image,
            resources: &Resources::default(),
            mounts: &[],
            bootstrap: &bootstrap,
            labels: &crate::runner::RunLabels::of(crate::runner::RunKind::Probe),
            shape: super::Shape::Probe,
        })
        .await
        .context("create")?;
    line("created", format!("{} in {}", handle.0, ms(started)));

    let outcome = exercise(provider.as_ref(), &server, &handle).await;
    match options.keep {
        true => line("kept", handle.0.clone()),
        false => {
            provider.destroy(&handle).await.context("destroy")?;
            line("destroyed", handle.0.clone());
        }
    }
    let caps = outcome?;
    if caps.route != Route::NodeOnly || caps.dns != Dns::None {
        bail!(
            "the sandbox has another way out: route {}, dns {}",
            caps.route.as_str(),
            caps.dns.as_str()
        );
    }
    println!("probe passed");
    Ok(())
}

/// The provider this pod's node would make its sandboxes with, from inside
/// its pod, on `bundle`; its listener bound now, so a port the running node
/// holds is said before any pod is made.
async fn kubernetes(
    cfg: &crate::config::Config,
    bundle: &std::path::Path,
) -> Result<KubeSandboxProvider> {
    let mut cfg = cfg.clone();
    cfg.sandbox.bundle = Some(bundle.to_path_buf());
    let backend = crate::boundary::kubernetes::KubeBackend::new(&cfg);
    let spec = backend
        .spec()
        .map_err(|why| anyhow::anyhow!("run the probe inside the node's pod: {why}"))?;
    let client = backend.client().await.map_err(anyhow::Error::msg)?;
    let provider = KubeSandboxProvider::new(client, KubeSandboxOptions::from_config(&cfg, spec));
    let listen = provider.options().listen;
    match crate::provider::kubernetes::Dispatcher::bind(listen).await {
        Ok(_) => Ok(provider),
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => bail!(
            "port {} is already taken, most likely by the node itself serving its sandboxes \
             ([runtime.kubernetes] sandbox_protocol is on): the probe is for a node about to \
             turn it on; with it on, the node's boundary check covers the same ground",
            listen.port()
        ),
        Err(error) => bail!("listen for the sandbox on {listen}: {error}"),
    }
}

async fn exercise(
    provider: &dyn SandboxProvider,
    server: &SandboxServer,
    handle: &Handle,
) -> Result<Capabilities> {
    let started = Instant::now();
    let conn = provider
        .connect(handle, server, CONNECT_WITHIN)
        .await
        .context("connect to the sandbox")?;
    let info = conn.info();
    line(
        "connected",
        format!(
            "in {}: tracon-sandbox {} on {}, pid {}{}",
            ms(started),
            info.version,
            info.arch,
            info.pid,
            if info.init { " (init)" } else { "" }
        ),
    );

    let started = Instant::now();
    let script = format!(
        "uname -sm; id -u; if touch {BUNDLE_TARGET}/.probe 2>/dev/null; then echo bundle-writable; else echo bundle-read-only; fi"
    );
    let output = conn
        .spawn(SpawnSpec::new(["sh", "-c", script.as_str()]))
        .await?
        .output()
        .await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    line(
        "ran",
        format!("in {}: {}", ms(started), stdout.trim().replace('\n', " | ")),
    );
    if !output.status.success() {
        bail!(
            "the command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if stdout.contains("bundle-writable") {
        bail!("the sandbox could write to the bundle");
    }

    let caps = provider
        .verify_isolation(handle)
        .await
        .context("verify isolation")?;
    line(
        "isolation",
        format!(
            "tier {}, route {}, dns {}, privilege {}, runtime socket {}, image {}",
            caps.tier.as_str(),
            caps.route.as_str(),
            caps.dns.as_str(),
            caps.privilege.as_str(),
            caps.runtime_socket.as_str(),
            caps.image_identity.as_str()
        ),
    );
    let network = provider.network(handle).await?;
    line("routes", network.destinations.join(", "));
    let image = provider.image_identity(handle).await?;
    line("image", format!("{image:?}"));
    conn.shutdown().await.context("shut the sandbox down")?;
    Ok(caps)
}

fn line(what: &str, detail: String) {
    println!("{what:<10} {detail}");
}

fn ms(since: Instant) -> String {
    format!("{} ms", since.elapsed().as_millis())
}
