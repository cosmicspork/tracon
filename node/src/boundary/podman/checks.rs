//! The Podman answers to the four startup checks (five with `--deep`). Each
//! is run against the same `RunSpec` a session uses, via a probe container
//! that is created but never started.

use super::{podman, podman_json};
use crate::boundary::checks::{egress_script, egress_verdict, CheckId, CheckResult, Denial};
use crate::boundary::{BoundaryError, BoundaryReport};
use crate::{
    config::Config,
    runner::{podman::RunSpec, RunnerCommand},
};

pub async fn check_all(cfg: &Config, selinux: bool, deep: bool) -> BoundaryReport {
    let mut checks = Vec::new();

    let runtime = check_runtime(cfg).await;
    let runtime_ok = runtime.ok;
    checks.push(runtime);

    if !runtime_ok {
        // Without a runtime the remaining checks cannot be answered honestly.
        for id in [
            CheckId::HarnessUnprivileged,
            CheckId::NoRuntimeSocket,
            CheckId::NetworkIsolated,
        ] {
            checks.push(CheckResult::fail(id, "not checked: runtime unavailable"));
        }
        return BoundaryReport { checks };
    }

    let mut spec = RunSpec::from_config(cfg, selinux);
    // Verify the same mount set a session runs, not a bare spec: the socket
    // check is only meaningful against what is actually mounted. The persistent
    // mounts (the harness state dir and credential db) are what every session
    // carries and are added here; the per-session worktree and git-dir mounts
    // are node-constructed from fixed paths and carry no daemon socket.
    spec.extra_mounts = crate::session::materialize::state_mounts(
        &spec.home,
        crate::adapter::layout(&cfg.harness.id),
    )
    .unwrap_or_default();
    let probe = crate::config::resource_name("boundary-probe");
    checks.extend(probe_checks(cfg, &spec, &probe).await);

    if deep {
        checks
            .push(check_egress(cfg, selinux, &crate::config::resource_name("egress-probe")).await);
    }
    BoundaryReport { checks }
}

async fn check_runtime(cfg: &Config) -> CheckResult {
    if let Err(e) = super::ensure_machine(cfg).await {
        return CheckResult::fail(CheckId::Runtime, e.to_string());
    }
    let info = match podman_json(&["info", "--format", "json"]).await {
        Ok(v) => v,
        Err(e) => return CheckResult::fail(CheckId::Runtime, format!("podman info: {e}")),
    };
    let rootless = info["host"]["security"]["rootless"]
        .as_bool()
        .unwrap_or(false);
    let version = info["version"]["Version"]
        .as_str()
        .unwrap_or("?")
        .to_string();
    if !rootless {
        return CheckResult::fail(
            CheckId::Runtime,
            format!("podman {version} is rootful; the harness must not share a root domain with the node"),
        );
    }
    // While the node is building them itself, the remedy is to wait; the
    // desktop app reads "run `tracon setup`" as its cue to run one.
    let building = crate::boundary::reconcile::running();
    for (image, dir) in super::setup::images(cfg) {
        let image = image.as_str();
        if podman(&["image", "exists", image]).await.is_err() {
            return CheckResult::fail(
                CheckId::Runtime,
                match building {
                    true => format!("image {image} is missing; the node is building it"),
                    false => format!("image {image} is missing; run `tracon setup`"),
                },
            );
        }
        // An image without the label predates it or was built by hand, and
        // cannot be judged; the next setup rebuilds and labels it.
        if let Some(built) = super::setup::image_digest(image).await {
            if built != super::setup::expected_digest(cfg, dir) {
                return CheckResult::fail(
                    CheckId::Runtime,
                    match building {
                        true => format!("image {image} is out of date; the node is rebuilding it"),
                        false => format!(
                            "image {image} was built from other definitions than this binary's; run `tracon setup`"
                        ),
                    },
                );
            }
        }
    }
    if super::uses_sandbox_protocol(cfg) {
        let bundle = crate::bundle::effective(cfg).unwrap_or_default();
        if !bundle.join("bin/tracon-sandbox").is_file() {
            return CheckResult::fail(
                CheckId::Runtime,
                match crate::bundle::managed(cfg) {
                    true if building => format!(
                        "the harness bundle {} is not here yet; the node is fetching it",
                        bundle.display()
                    ),
                    true => format!(
                        "the harness bundle {} is not here; the node fetches it from {} when it sets up its boundary (`tracon setup`)",
                        bundle.display(),
                        cfg.sandbox.release_url()
                    ),
                    false => format!(
                        "[runtime.podman] sandbox_protocol is on, and [sandbox] bundle {:?} has no bin/tracon-sandbox",
                        bundle.display().to_string()
                    ),
                },
            );
        }
        if crate::sandbox_images::enabled(cfg) {
            let base = crate::sandbox_images::base_image(cfg);
            if podman(&["image", "exists", &base]).await.is_err() {
                return CheckResult::fail(
                    CheckId::Runtime,
                    match building {
                        true => format!("image {base} is missing; the node is pulling it"),
                        false => format!("image {base} is missing; run `tracon setup`"),
                    },
                );
            }
            return CheckResult::ok(
                CheckId::Runtime,
                format!(
                    "podman {version}, rootless, images present; sandboxes run {base} with the harnesses from {}",
                    bundle.display()
                ),
            );
        }
        if let Some(missing) = crate::provider::podman::service_bundles(cfg)
            .into_iter()
            .map(|(bundle, _)| bundle)
            .find(|bundle| !bundle.is_dir())
        {
            return CheckResult::fail(
                CheckId::Runtime,
                format!(
                    "[runtime.podman] sandbox_services is on, and service bundle {:?} is not a directory",
                    missing.display().to_string()
                ),
            );
        }
        let services = match super::uses_sandbox_services(cfg) {
            true => ", services in the sandbox",
            false => "",
        };
        return CheckResult::ok(
            CheckId::Runtime,
            format!(
                "podman {version}, rootless, images present; commands run over the sandbox protocol from {}{}{services}",
                bundle.display(),
                match super::uses_sandbox_tunnels(cfg) {
                    true => ", reaching the node through tunnels with no gateway container",
                    false => "",
                }
            ),
        );
    }
    CheckResult::ok(
        CheckId::Runtime,
        format!("podman {version}, rootless, images present"),
    )
}

/// The checks a probe container answers (`harness_unprivileged`,
/// `no_runtime_socket`, `network_isolated`), against one created as
/// sessions run under `cfg`, named `probe`, and removed afterwards.
pub async fn probe_checks(cfg: &Config, spec: &RunSpec, probe: &str) -> Vec<CheckResult> {
    let created = match super::uses_sandbox_protocol(cfg) {
        true => create_sandbox_probe(cfg, spec, probe).await,
        false => create_probe(spec, probe).await,
    };
    let mut checks = Vec::new();
    match created {
        Ok(inspect) => {
            checks.push(check_unprivileged(&inspect));
            checks.push(check_no_runtime_socket(&inspect));
            checks.push(check_network(cfg, &inspect).await);
            let _ = podman(&["rm", "-f", "-i", probe]).await;
        }
        Err(e) => {
            for id in [
                CheckId::HarnessUnprivileged,
                CheckId::NoRuntimeSocket,
                CheckId::NetworkIsolated,
            ] {
                checks.push(CheckResult::fail(
                    id,
                    format!("probe container failed: {e}"),
                ));
            }
        }
    }
    checks
}

/// Create (do not start) a container from the real session spec, and return its
/// inspect JSON.
async fn create_probe(spec: &RunSpec, probe: &str) -> Result<serde_json::Value, BoundaryError> {
    let _ = podman(&["rm", "-f", "-i", probe]).await;
    let cmd = RunnerCommand {
        argv: vec!["true".into()],
        ..Default::default()
    };
    let args = spec.podman_args(probe, &cmd, true);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    podman(&argv).await?;
    let v = podman_json(&["inspect", probe, "--format", "json"]).await?;
    Ok(v.get(0).cloned().unwrap_or(serde_json::Value::Null))
}

/// The probe for a node running its commands over the sandbox protocol: a
/// sandbox container as the provider makes one, with the bundle and the
/// socket directory mounted, created and not started.
async fn create_sandbox_probe(
    cfg: &Config,
    spec: &RunSpec,
    probe: &str,
) -> Result<serde_json::Value, BoundaryError> {
    use crate::provider::{Bootstrap, CreateSpec, Resources};
    let _ = podman(&["rm", "-f", "-i", probe]).await;
    let mut boundary = spec.clone();
    let mounts = std::mem::take(&mut boundary.extra_mounts);
    let provider = crate::provider::podman::PodmanProvider::from_config(cfg, boundary);
    let pin = crate::runner::protocol::node_server()
        .map_err(|e| BoundaryError::Other(e.to_string()))?
        .pin()
        .to_string();
    let bootstrap = Bootstrap {
        run: probe.into(),
        cert_pin: pin,
        token: String::new(),
    };
    let create = CreateSpec {
        name: Some(probe),
        image: &spec.image,
        resources: &Resources::default(),
        mounts: &mounts,
        bootstrap: &bootstrap,
    };
    let bundle = &provider.options().bundle;
    if !bundle.join("bin/tracon-sandbox").is_file() {
        return Err(BoundaryError::Other(format!(
            "[sandbox] bundle {} has no bin/tracon-sandbox",
            bundle.display()
        )));
    }
    provider
        .prepare_dir(probe)
        .map_err(|e| BoundaryError::Other(e.to_string()))?;
    let entry = provider
        .entry_for(&spec.image)
        .await
        .map_err(|e| BoundaryError::Other(e.to_string()))?;
    let args = provider.run_args(probe, &create, entry, true);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    podman(&argv).await?;
    let v = podman_json(&["inspect", probe, "--format", "json"]).await?;
    let _ = std::fs::remove_dir_all(provider.socket_dir(probe));
    Ok(v.get(0).cloned().unwrap_or(serde_json::Value::Null))
}

fn check_unprivileged(c: &serde_json::Value) -> CheckResult {
    let hc = &c["HostConfig"];
    if hc["Privileged"].as_bool().unwrap_or(true) {
        return CheckResult::fail(CheckId::HarnessUnprivileged, "container is privileged");
    }
    let cap_add = hc["CapAdd"].as_array().map(|a| a.len()).unwrap_or(0);
    if cap_add > 0 {
        return CheckResult::fail(
            CheckId::HarnessUnprivileged,
            format!("container adds {cap_add} capabilities"),
        );
    }
    let eff = c["EffectiveCaps"].as_array().map(|a| a.len()).unwrap_or(0);
    if eff > 0 {
        return CheckResult::fail(
            CheckId::HarnessUnprivileged,
            format!("container retains {eff} effective capabilities"),
        );
    }
    let nnp = hc["SecurityOpt"]
        .as_array()
        .map(|a| a.iter().any(|s| s.as_str().unwrap_or("").contains("no-new-privileges")))
        .unwrap_or(false)
        // Podman also reports it here once applied.
        || c["Config"]["Annotations"]["run.oci.no_new_privileges"] == "true"
        || hc["NoNewPrivileges"].as_bool().unwrap_or(false);
    if !nnp {
        return CheckResult::fail(
            CheckId::HarnessUnprivileged,
            "no-new-privileges is not set on the harness container",
        );
    }
    CheckResult::ok(
        CheckId::HarnessUnprivileged,
        "unprivileged, no capabilities, no-new-privileges",
    )
}

fn check_no_runtime_socket(c: &serde_json::Value) -> CheckResult {
    let mut offenders = Vec::new();
    let mut check = |s: &str| {
        let l = s.to_ascii_lowercase();
        if l.ends_with(".sock") || l.contains("docker.sock") || l.contains("podman.sock") {
            offenders.push(s.to_string());
        }
    };
    if let Some(mounts) = c["Mounts"].as_array() {
        for m in mounts {
            if let Some(s) = m["Source"].as_str() {
                check(s);
            }
            if let Some(d) = m["Destination"].as_str() {
                check(d);
            }
        }
    }
    if let Some(binds) = c["HostConfig"]["Binds"].as_array() {
        for b in binds {
            if let Some(s) = b.as_str() {
                check(s);
            }
        }
    }
    if offenders.is_empty() {
        CheckResult::ok(
            CheckId::NoRuntimeSocket,
            "no container runtime socket reachable from the harness",
        )
    } else {
        CheckResult::fail(
            CheckId::NoRuntimeSocket,
            format!("socket mounted into the harness: {}", offenders.join(", ")),
        )
    }
}

async fn check_network(cfg: &Config, c: &serde_json::Value) -> CheckResult {
    if super::uses_sandbox_tunnels(cfg) {
        return check_node_only(cfg, c);
    }
    let net = match podman_json(&[
        "network",
        "inspect",
        &cfg.boundary.network,
        "--format",
        "json",
    ])
    .await
    {
        Ok(v) => v.get(0).cloned().unwrap_or(serde_json::Value::Null),
        Err(e) => {
            return CheckResult::fail(
                CheckId::NetworkIsolated,
                format!("network {} not found: {e}", cfg.boundary.network),
            )
        }
    };
    if !net["internal"].as_bool().unwrap_or(false) {
        return CheckResult::fail(
            CheckId::NetworkIsolated,
            format!(
                "{} is not internal: the harness has a route out",
                cfg.boundary.network
            ),
        );
    }
    if net["dns_enabled"].as_bool().unwrap_or(true) {
        return CheckResult::fail(
            CheckId::NetworkIsolated,
            format!("{} has DNS enabled; disable it", cfg.boundary.network),
        );
    }
    // The harness container must be on that network and nothing else.
    let nets: Vec<String> = c["NetworkSettings"]["Networks"]
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    if nets != vec![cfg.boundary.network.clone()] {
        return CheckResult::fail(
            CheckId::NetworkIsolated,
            format!(
                "harness is attached to {nets:?}, expected only {:?}",
                cfg.boundary.network
            ),
        );
    }
    // And the gateway has to be up, or the harness reaches nothing at all.
    let gw = match podman_json(&[
        "inspect",
        &cfg.boundary.gateway_container,
        "--format",
        "json",
    ])
    .await
    {
        Ok(v) => v.get(0).cloned().unwrap_or(serde_json::Value::Null),
        Err(e) => {
            return CheckResult::fail(
                CheckId::NetworkIsolated,
                format!(
                    "gateway {} is not running: {e}",
                    cfg.boundary.gateway_container
                ),
            )
        }
    };
    if gw["State"]["Running"].as_bool() != Some(true) {
        return CheckResult::fail(
            CheckId::NetworkIsolated,
            format!("gateway {} is not running", cfg.boundary.gateway_container),
        );
    }
    let gw_ip = gw["NetworkSettings"]["Networks"][&cfg.boundary.network]["IPAddress"]
        .as_str()
        .unwrap_or("");
    if gw_ip != cfg.boundary.gateway_ip {
        return CheckResult::fail(
            CheckId::NetworkIsolated,
            format!(
                "gateway is at {gw_ip:?} on {}, expected {}",
                cfg.boundary.network, cfg.boundary.gateway_ip
            ),
        );
    }
    CheckResult::ok(
        CheckId::NetworkIsolated,
        format!(
            "{} is internal with no DNS; gateway at {}",
            cfg.boundary.network, cfg.boundary.gateway_ip
        ),
    )
}

/// `network_isolated` for a sandbox whose only route is the node, read back
/// from `podman inspect`: no network but Podman's `none`, no host path
/// mounted but the bundle and the socket directory, no resolver, and the
/// gateway's name on loopback, where the tunnels listen. No gateway
/// container or internal network is involved.
fn check_node_only(cfg: &Config, c: &serde_json::Value) -> CheckResult {
    let hc = &c["HostConfig"];
    let mode = hc["NetworkMode"].as_str().unwrap_or("");
    let nets: Vec<String> = c["NetworkSettings"]["Networks"]
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    if mode != "none" || nets.iter().any(|n| n != "none") {
        return CheckResult::fail(
            CheckId::NetworkIsolated,
            format!(
                "the sandbox has network {mode:?} ({nets:?}); with sandbox_tunnels it must have none"
            ),
        );
    }
    let binds = crate::provider::podman::other_binds(c);
    if !binds.is_empty() {
        return CheckResult::fail(
            CheckId::NetworkIsolated,
            format!(
                "host paths are bind-mounted into the sandbox at {}; only the bundle, the socket directory and read-only service bundles may be",
                binds.join(", ")
            ),
        );
    }
    let socket_dir = c["Mounts"].as_array().and_then(|mounts| {
        mounts
            .iter()
            .find(|m| m["Destination"].as_str() == Some(crate::runner::podman::SOCKET_DIR_TARGET))
    });
    match socket_dir {
        Some(m) if m["RW"].as_bool() == Some(false) => {}
        Some(_) => {
            return CheckResult::fail(
                CheckId::NetworkIsolated,
                "the socket directory is mounted writable",
            )
        }
        None => {
            return CheckResult::fail(
                CheckId::NetworkIsolated,
                "the sandbox has no socket directory: it could not reach the node",
            )
        }
    }
    if hc["Dns"].as_array().is_some_and(|d| !d.is_empty()) {
        return CheckResult::fail(CheckId::NetworkIsolated, "the sandbox is given a resolver");
    }
    let gateway = &cfg.boundary.gateway_container;
    let loopback = format!("{gateway}:127.0.0.1");
    let hosts: Vec<&str> = hc["ExtraHosts"]
        .as_array()
        .map(|a| a.iter().filter_map(|h| h.as_str()).collect())
        .unwrap_or_default();
    if !hosts.contains(&loopback.as_str()) {
        return CheckResult::fail(
            CheckId::NetworkIsolated,
            format!("{gateway} is not the sandbox's loopback ({hosts:?}): the harness would not find the tunnels"),
        );
    }
    CheckResult::ok(
        CheckId::NetworkIsolated,
        format!(
            "route: node-only, dns: none. No network (podman `none`): the one way out is the node's \
             socket, mounted read-only at {}; {gateway} is loopback, where the MCP and model gateway \
             and both proxies are tunnels to the node; no gateway container or internal network",
            crate::runner::podman::SOCKET_DIR_TARGET
        ),
    )
}

const ANONYMOUS_GRANT: &str = "GRANT_ANONYMOUS_OK";

/// What only a sandbox with no network must also show from inside: no name
/// resolves, and the internal network's gateway address, where a harness
/// container would have found tinyproxy, is not there.
fn node_only_script(cfg: &Config) -> String {
    format!(
        "getent hosts example.com >/dev/null 2>&1 && echo DIRECT_DNS_OK; \
         curl -s -o /dev/null -m 5 --noproxy '*' http://{}:{}/ && echo DIRECT_GATEWAY_OK; ",
        cfg.boundary.gateway_ip, cfg.gateway.proxy_port
    )
}

/// The verdict for a node-only sandbox: what `egress_verdict` asks of every
/// boundary, and then that nothing resolved and the old gateway was not
/// there.
fn node_only_verdict(out: &str) -> CheckResult {
    if out.contains("DIRECT_DNS_OK") {
        return CheckResult::fail(CheckId::Egress, "a name resolved inside the sandbox");
    }
    if out.contains("DIRECT_GATEWAY_OK") {
        return CheckResult::fail(
            CheckId::Egress,
            "the sandbox reached the internal network's gateway address",
        );
    }
    let verdict = egress_verdict(out, Denial::Rejects);
    match verdict.ok {
        true => CheckResult::ok(
            CheckId::Egress,
            format!(
                "{}; from inside, no name resolves and nothing answers but the tunnels: \
                 the node answered through the gateway tunnel and the allowlist proxy's",
                verdict.detail
            ),
        ),
        false => verdict,
    }
}

/// Actively prove the boundary from inside it: no direct egress, allowlisted
/// hosts reachable through the proxy, unlisted hosts refused. `probe` names
/// the container it runs in.
pub async fn check_egress(cfg: &Config, selinux: bool, probe: &str) -> CheckResult {
    let spec = RunSpec::from_config(cfg, selinux);
    let node_only = super::uses_sandbox_tunnels(cfg);
    // The per-client proxy must serve nobody who presents no grant. A node
    // that is not running answers nothing at all, which proves the same. Asked
    // first: the script's last command is what decides its exit status.
    let script = format!(
        "{}curl -s -o /dev/null -m 10 -x http://{}:{} https://example.com/ && echo {ANONYMOUS_GRANT}; {}",
        if node_only { node_only_script(cfg) } else { String::new() },
        cfg.boundary.gateway_container,
        cfg.gateway.qa_proxy_port,
        egress_script(&cfg.boundary.gateway_container, cfg.gateway.forward_port),
    );
    let cmd = RunnerCommand {
        argv: vec!["sh".into(), "-c".into(), script],
        ..Default::default()
    };
    // Clear a leftover of the same name from a crashed prior `--deep` run, or
    // this run fails with a name conflict instead of re-probing.
    let _ = podman(&["rm", "-f", "-i", probe]).await;
    if super::uses_sandbox_protocol(cfg) {
        // Run as everything else runs: in a sandbox, over the protocol, and
        // with sandbox_tunnels through the tunnels a session gets.
        let runner = super::protocol_runner(cfg, spec);
        let cmd = RunnerCommand {
            name: probe.into(),
            ..cmd
        };
        return match runner.run_capture(cmd).await {
            Ok(out) if out.status.success() => {
                let out = String::from_utf8_lossy(&out.stdout);
                match (out.contains(ANONYMOUS_GRANT), node_only) {
                    (true, _) => CheckResult::fail(
                        CheckId::Egress,
                        "the per-client egress proxy served a request that presented no grant",
                    ),
                    (false, true) => node_only_verdict(&out),
                    (false, false) => egress_verdict(&out, Denial::Rejects),
                }
            }
            Ok(out) => CheckResult::fail(
                CheckId::Egress,
                format!(
                    "probe failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            ),
            Err(e) => CheckResult::fail(CheckId::Egress, format!("probe failed: {e}")),
        };
    }
    let args = spec.podman_args(probe, &cmd, false);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    match podman(&argv).await {
        // The internal network has no route out at all, so a denied
        // destination fails at `connect(2)`. The probe is what proves it.
        Ok(out) if out.contains(ANONYMOUS_GRANT) => CheckResult::fail(
            CheckId::Egress,
            "the per-client egress proxy served a request that presented no grant",
        ),
        Ok(out) => egress_verdict(&out, Denial::Rejects),
        Err(e) => CheckResult::fail(CheckId::Egress, format!("probe failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn good_probe() -> serde_json::Value {
        json!({
            "HostConfig": {"Privileged": false, "CapAdd": [], "SecurityOpt": ["no-new-privileges"], "Binds": []},
            "EffectiveCaps": [],
            "Mounts": [{"Source": "/state", "Destination": "/root/.claude"}],
            "NetworkSettings": {"Networks": {"tracon-int": {"IPAddress": "10.89.0.5"}}}
        })
    }

    #[test]
    fn privileged_container_fails() {
        let mut c = good_probe();
        c["HostConfig"]["Privileged"] = json!(true);
        let r = check_unprivileged(&c);
        assert!(!r.ok);
        assert!(r.detail.contains("privileged"));
    }

    #[test]
    fn retained_capabilities_fail() {
        let mut c = good_probe();
        c["EffectiveCaps"] = json!(["CAP_CHOWN", "CAP_SYS_ADMIN"]);
        assert!(!check_unprivileged(&c).ok);
    }

    #[test]
    fn missing_no_new_privileges_fails() {
        let mut c = good_probe();
        c["HostConfig"]["SecurityOpt"] = json!([]);
        assert!(!check_unprivileged(&c).ok);
    }

    #[test]
    fn a_clean_probe_passes() {
        assert!(check_unprivileged(&good_probe()).ok);
        assert!(check_no_runtime_socket(&good_probe()).ok);
    }

    fn node_only_probe() -> serde_json::Value {
        json!({
            "HostConfig": {"Privileged": false, "CapAdd": [], "SecurityOpt": ["no-new-privileges"],
                "NetworkMode": "none", "Dns": [], "ExtraHosts": ["tracon-gw:127.0.0.1"]},
            "EffectiveCaps": [],
            "Mounts": [
                {"Type": "bind", "Source": "/b", "Destination": "/opt/tracon", "RW": false},
                {"Type": "bind", "Source": "/run/user/1/tracon/sandbox/ab", "Destination": "/run/tracon", "RW": false},
                {"Type": "volume", "Source": "/v", "Destination": "/root/.claude", "RW": true},
            ],
            "NetworkSettings": {"Networks": {"none": {"IPAddress": ""}}}
        })
    }

    #[test]
    fn a_node_only_sandbox_passes_on_its_single_route_and_nothing_else() {
        let cfg = Config::default();
        let ok = check_node_only(&cfg, &node_only_probe());
        assert!(ok.ok, "{}", ok.detail);
        assert!(
            ok.detail.starts_with("route: node-only, dns: none"),
            "{}",
            ok.detail
        );

        let cases: [(&str, serde_json::Value, &str); 6] = [
            ("/HostConfig/NetworkMode", json!("bridge"), "must have none"),
            (
                "/NetworkSettings/Networks",
                json!({"tracon-int": {}}),
                "must have none",
            ),
            ("/Mounts/0/Destination", json!("/srv"), "bind-mounted"),
            ("/Mounts/1/RW", json!(true), "writable"),
            ("/HostConfig/Dns", json!(["1.1.1.1"]), "resolver"),
            (
                "/HostConfig/ExtraHosts",
                json!(["tracon-gw:10.89.0.2"]),
                "loopback",
            ),
        ];
        for (pointer, value, expect) in cases {
            let mut c = node_only_probe();
            *c.pointer_mut(pointer).unwrap() = value;
            let r = check_node_only(&cfg, &c);
            assert!(!r.ok, "{pointer}");
            assert!(r.detail.contains(expect), "{pointer}: {}", r.detail);
        }
        let mut c = node_only_probe();
        c["Mounts"].as_array_mut().unwrap().remove(1);
        assert!(check_node_only(&cfg, &c)
            .detail
            .contains("no socket directory"));
    }

    #[test]
    fn from_inside_a_node_only_sandbox_a_name_or_the_old_gateway_fails_the_egress_check() {
        let good = "DIRECT_NAME_SECONDS=0\nDIRECT_ADDR_SECONDS=0\nPROXY_ALLOWED\npong";
        let r = node_only_verdict(good);
        assert!(r.ok, "{}", r.detail);
        assert!(r.detail.contains("no name resolves"), "{}", r.detail);
        for marker in ["DIRECT_DNS_OK", "DIRECT_GATEWAY_OK", "PROXY_UNLISTED"] {
            assert!(
                !node_only_verdict(&format!("{marker}\n{good}")).ok,
                "{marker}"
            );
        }
        // Without the tunnels nothing answers, and that is no pass either.
        assert!(!node_only_verdict("DIRECT_NAME_SECONDS=0\nDIRECT_ADDR_SECONDS=0").ok);
        let script = node_only_script(&Config::default());
        assert!(script.contains("getent hosts"), "{script}");
    }

    #[test]
    fn a_mounted_runtime_socket_fails() {
        let mut c = good_probe();
        c["Mounts"] = json!([{"Source": "/run/user/501/podman/podman.sock", "Destination": "/var/run/docker.sock"}]);
        let r = check_no_runtime_socket(&c);
        assert!(!r.ok);
        assert!(r.detail.contains("podman.sock"));
    }
}
