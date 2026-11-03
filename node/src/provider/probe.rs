//! `tracon sandbox probe`: make one sandbox with a provider, connect to it
//! over the sandbox protocol, run a command in it, read its isolation back
//! from the provider, and remove it. A check that a provider works on this
//! host, end to end, before any session depends on it.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use tracon_sandbox::node::{SandboxServer, ServerOptions};
use tracon_sandbox::protocol::SpawnSpec;
use tracon_sandbox::tls::NodeIdentity;

use super::apple::{AppleOptions, AppleProvider, BUNDLE_TARGET};
use super::capabilities::{Dns, Route};
use super::podman::PodmanProvider;
use super::{
    Bootstrap, Capabilities, CreateSpec, Handle, ProviderKind, Resources, SandboxProvider,
};

/// How long the sandbox has to start and take the node's connection.
const CONNECT_WITHIN: Duration = Duration::from_secs(60);

pub struct ProbeOptions {
    pub provider: ProviderKind,
    pub image: String,
    /// The harness bundle for the provider's architecture, holding
    /// `bin/tracon-sandbox`.
    pub bundle: PathBuf,
    /// Leave the sandbox running afterwards, for a look inside.
    pub keep: bool,
    /// What to call the sandbox, where the provider names it as asked.
    pub name: Option<String>,
}

/// What one probe saw. Each line is printed as it happens.
pub async fn run(options: ProbeOptions) -> Result<()> {
    let bundle = options
        .bundle
        .canonicalize()
        .with_context(|| format!("bundle {}", options.bundle.display()))?;
    let provider: Box<dyn SandboxProvider> = match options.provider {
        ProviderKind::Apple => Box::new(AppleProvider::new(AppleOptions {
            bundle,
            socket_dir: AppleOptions::default_socket_dir(),
            limit: None,
            ..AppleOptions::default()
        })),
        // The boundary this node's configuration describes: its network,
        // gateway and proxy, as a session's sandbox gets them.
        ProviderKind::Podman => {
            let mut cfg = crate::config::Config::load();
            cfg.sandbox.bundle = Some(bundle);
            let spec = crate::runner::podman::RunSpec::from_config(&cfg, false);
            Box::new(PodmanProvider::from_config(&cfg, spec))
        }
        other => bail!("the probe does not support {} yet", other.as_str()),
    };
    let supervisor = options.bundle.join("bin/tracon-sandbox");
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
            image: &options.image,
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
