//! The five checks on a Mac node: `container` and what every sandbox starts
//! from, then one probe sandbox made as a session's is, run over the
//! protocol and read back from `container inspect`, and with `deep` the
//! egress script run inside one.

use std::time::Duration;

use tracon_sandbox::protocol::SpawnSpec;

use super::AppleBackend;
use crate::boundary::checks::{egress_script, egress_verdict, Denial};
use crate::boundary::{BoundaryReport, CheckId, CheckResult};
use crate::config::Config;
use crate::provider::apple::{AppleProvider, BUNDLE_TARGET};
use crate::provider::capabilities::{Capabilities, Dns, Route, RuntimeSocket, Tier};
use crate::provider::{Bootstrap, CreateSpec, Handle, Resources, SandboxProvider as _, Shape};
use crate::runner::{RunKind, RunLabels, RunnerCommand};

/// How long the probe sandbox has to start and take the node's connection.
const PROBE_WITHIN: Duration = Duration::from_secs(60);

const UNCHECKED: [CheckId; 3] = [
    CheckId::HarnessUnprivileged,
    CheckId::NoRuntimeSocket,
    CheckId::NetworkIsolated,
];

pub async fn check_all(b: &AppleBackend, cfg: &Config, deep: bool) -> BoundaryReport {
    let mut checks = Vec::new();
    let runtime = check_runtime(b, cfg).await;
    let runtime_ok = runtime.ok;
    checks.push(runtime);
    if !runtime_ok {
        for id in UNCHECKED {
            checks.push(CheckResult::fail(id, "not checked: runtime unavailable"));
        }
        return BoundaryReport { checks };
    }
    match probe(b, cfg).await {
        Ok(seen) => checks.extend(verdicts(&seen)),
        Err(error) => {
            for id in UNCHECKED {
                checks.push(CheckResult::fail(id, format!("probe sandbox: {error}")));
            }
        }
    }
    if deep {
        checks.push(check_egress(b, cfg).await);
    }
    BoundaryReport { checks }
}

/// `container` running, the bundle with its supervisor, and the base pinned.
async fn check_runtime(b: &AppleBackend, cfg: &Config) -> CheckResult {
    // The bundle checked and the base pinned in this process, as the node's
    // start does: a check run on its own has to find them itself.
    if let Err(why) = b.converge_images().await {
        return CheckResult::fail(CheckId::Runtime, why);
    }
    let Some(bundle) = crate::bundle::effective(cfg) else {
        return CheckResult::fail(
            CheckId::Runtime,
            "no harness bundle: set [sandbox] bundle, or let the node fetch its release's",
        );
    };
    if !bundle.join("bin/tracon-sandbox").is_file() {
        return CheckResult::fail(
            CheckId::Runtime,
            format!(
                "{} has no bin/tracon-sandbox for linux/arm64",
                bundle.display()
            ),
        );
    }
    if let Some(missing) = crate::service_bundle::missing(cfg, false) {
        return CheckResult::fail(CheckId::Runtime, missing);
    }
    let base = crate::sandbox_images::base_image(cfg);
    if !base.contains("@sha256:") {
        return CheckResult::fail(
            CheckId::Runtime,
            format!("the base image {base} is not pulled and pinned yet"),
        );
    }
    CheckResult::ok(
        CheckId::Runtime,
        format!(
            "container is running; each sandbox is a VM of its own from {base}, with the bundle \
             {} mounted read-only",
            bundle.display()
        ),
    )
}

/// What the probe sandbox showed.
struct Seen {
    /// `id -u` inside, and whether the bundle could be written.
    uid: String,
    bundle_writable: bool,
    caps: Capabilities,
    destinations: Vec<String>,
}

/// One sandbox as a session's is made, its isolation read back, and gone.
async fn probe(b: &AppleBackend, cfg: &Config) -> Result<Seen, String> {
    let provider = b.provider(&cfg.harness.id);
    let server = crate::runner::protocol::node_server().map_err(|e| e.to_string())?;
    let run = format!("probe:{}", uuid::Uuid::now_v7().simple());
    let bootstrap = Bootstrap {
        run: run.clone(),
        cert_pin: server.pin().to_string(),
        token: server.tokens().issue(run.clone()),
    };
    let name = crate::config::resource_name("probe");
    let created = provider
        .create(&CreateSpec {
            name: Some(&name),
            image: &crate::sandbox_images::base_image(cfg),
            resources: &Resources::default(),
            mounts: &[],
            bootstrap: &bootstrap,
            labels: &RunLabels::of(RunKind::Probe),
            shape: Shape::Probe,
        })
        .await;
    let handle = match created {
        Ok(handle) => handle,
        Err(error) => {
            server.tokens().revoke(&run);
            return Err(error.to_string());
        }
    };
    let seen = look(&provider, &server, &handle).await;
    server.tokens().revoke(&run);
    if let Err(error) = provider.destroy(&handle).await {
        tracing::warn!(sandbox = %handle.0, %error, "could not remove the probe sandbox");
    }
    seen
}

async fn look(
    provider: &AppleProvider,
    server: &tracon_sandbox::node::SandboxServer,
    handle: &Handle,
) -> Result<Seen, String> {
    let conn = provider
        .connect(handle, server, PROBE_WITHIN)
        .await
        .map_err(|e| e.to_string())?;
    let script = format!(
        "id -u; if touch {BUNDLE_TARGET}/.probe 2>/dev/null; then echo bundle-writable; fi"
    );
    let output = conn
        .spawn(SpawnSpec::new(["sh", "-c", script.as_str()]))
        .await
        .map_err(|e| format!("run in the sandbox: {e}"))?
        .output()
        .await
        .map_err(|e| format!("run in the sandbox: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let caps = provider
        .verify_isolation(handle)
        .await
        .map_err(|e| e.to_string())?;
    let destinations = provider
        .network(handle)
        .await
        .map_err(|e| e.to_string())?
        .destinations;
    let _ = conn.shutdown().await;
    Ok(Seen {
        uid: stdout.lines().next().unwrap_or_default().trim().to_string(),
        bundle_writable: stdout.contains("bundle-writable"),
        caps,
        destinations,
    })
}

fn verdicts(seen: &Seen) -> Vec<CheckResult> {
    let mut checks = Vec::new();
    let in_vm = seen.caps.tier == Tier::Microvm;
    checks.push(match (in_vm, seen.uid.as_str(), seen.bundle_writable) {
        (_, _, true) => CheckResult::fail(
            CheckId::HarnessUnprivileged,
            "the sandbox could write to the harness bundle",
        ),
        (false, _, _) => CheckResult::fail(
            CheckId::HarnessUnprivileged,
            "the sandbox is not a VM of its own",
        ),
        (true, "0", _) => CheckResult::ok(
            CheckId::HarnessUnprivileged,
            "root inside a VM of its own, with the bundle read-only",
        ),
        (true, uid, _) => CheckResult::ok(
            CheckId::HarnessUnprivileged,
            format!("uid {uid} inside a VM of its own, with the bundle read-only"),
        ),
    });
    checks.push(match seen.caps.runtime_socket {
        RuntimeSocket::Absent => CheckResult::ok(
            CheckId::NoRuntimeSocket,
            "container's API is reached over XPC on the host; nothing relays it into the VM",
        ),
        RuntimeSocket::Present => CheckResult::fail(
            CheckId::NoRuntimeSocket,
            "a runtime socket is reachable from the sandbox",
        ),
    });
    checks.push(match (seen.caps.route, seen.caps.dns) {
        (Route::NodeOnly, Dns::None) => CheckResult::ok(
            CheckId::NetworkIsolated,
            format!(
                "no network and no resolver; the only way out is {}",
                seen.destinations.join(", ")
            ),
        ),
        (route, dns) => CheckResult::fail(
            CheckId::NetworkIsolated,
            format!(
                "route {}, dns {}: {}",
                route.as_str(),
                dns.as_str(),
                seen.destinations.join(", ")
            ),
        ),
    });
    checks
}

/// The shared egress script in a sandbox made as a session's: no direct way
/// out, the allowlist through its tunnel, the node through the forward.
async fn check_egress(b: &AppleBackend, cfg: &Config) -> CheckResult {
    let cmd = RunnerCommand {
        argv: vec![
            "sh".into(),
            "-c".into(),
            egress_script(super::GATEWAY_HOST, cfg.gateway.forward_port),
        ],
        name: crate::config::resource_name("egress-probe"),
        labels: RunLabels::of(RunKind::Probe),
        ..Default::default()
    };
    let runner = crate::boundary::Backend::runner(b, Vec::new());
    match runner.run_probe(cmd).await {
        Ok(out) => egress_verdict(&String::from_utf8_lossy(&out.stdout), Denial::Rejects),
        Err(e) => CheckResult::fail(CheckId::Egress, format!("probe failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::capabilities::{ImageIdentityKind, Privilege};

    fn seen(uid: &str, writable: bool, route: Route, dns: Dns) -> Seen {
        Seen {
            uid: uid.into(),
            bundle_writable: writable,
            caps: Capabilities {
                tier: Tier::Microvm,
                route,
                dns,
                privilege: Privilege::RootInVm,
                runtime_socket: RuntimeSocket::Absent,
                image_identity: ImageIdentityKind::Exact,
            },
            destinations: vec!["unix:/tmp/tracon-501/x.sock".into()],
        }
    }

    #[test]
    fn a_node_only_vm_passes() {
        let checks = verdicts(&seen("65532", false, Route::NodeOnly, Dns::None));
        assert!(checks.iter().all(|c| c.ok), "{checks:?}");
        assert!(checks[0].detail.contains("uid 65532"));
        assert!(checks[2].detail.contains("unix:/tmp/tracon-501/x.sock"));
    }

    #[test]
    fn another_way_out_or_a_writable_bundle_fails() {
        let open = verdicts(&seen("65532", false, Route::Unverified, Dns::ProviderOpen));
        assert!(!open[2].ok);
        let writable = verdicts(&seen("65532", true, Route::NodeOnly, Dns::None));
        assert!(!writable[0].ok);
        let mut shared = seen("0", false, Route::NodeOnly, Dns::None);
        shared.caps.tier = Tier::Container;
        assert!(!verdicts(&shared)[0].ok);
    }
}
