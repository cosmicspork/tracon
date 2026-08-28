//! The Podman answers to the four startup checks (five with `--deep`). Each
//! is run against the same `RunSpec` a session uses, via a probe container
//! that is created but never started.

use super::{podman, podman_json};
use crate::boundary::checks::{egress_script, egress_verdict, CheckId, CheckResult};
use crate::boundary::{BoundaryError, BoundaryReport};
use crate::{
    config::Config,
    runner::{podman::RunSpec, RunnerCommand},
};

const PROBE: &str = "tracon-boundary-probe";

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
    spec.extra_mounts = crate::session::materialize::state_mounts(&spec.home).unwrap_or_default();
    match create_probe(&spec).await {
        Ok(inspect) => {
            checks.push(check_unprivileged(&inspect));
            checks.push(check_no_runtime_socket(&inspect));
            checks.push(check_network(cfg, &inspect).await);
            let _ = podman(&["rm", "-f", "-i", PROBE]).await;
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

    if deep {
        checks.push(check_egress(cfg, selinux).await);
    }
    BoundaryReport { checks }
}

async fn check_runtime(cfg: &Config) -> CheckResult {
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
    for image in [&cfg.boundary.harness_image, &cfg.boundary.gateway_image] {
        if podman(&["image", "exists", image]).await.is_err() {
            return CheckResult::fail(
                CheckId::Runtime,
                format!("image {image} is missing; run `tracon setup`"),
            );
        }
    }
    CheckResult::ok(
        CheckId::Runtime,
        format!("podman {version}, rootless, images present"),
    )
}

/// Create (do not start) a container from the real session spec, and return its
/// inspect JSON.
async fn create_probe(spec: &RunSpec) -> Result<serde_json::Value, BoundaryError> {
    let _ = podman(&["rm", "-f", "-i", PROBE]).await;
    let cmd = RunnerCommand {
        argv: vec!["true".into()],
        ..Default::default()
    };
    let args = spec.podman_args(PROBE, &cmd, true);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    podman(&argv).await?;
    let v = podman_json(&["inspect", PROBE, "--format", "json"]).await?;
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

/// Actively prove the boundary from inside it: no direct egress, allowlisted
/// hosts reachable through the proxy, unlisted hosts refused.
async fn check_egress(cfg: &Config, selinux: bool) -> CheckResult {
    let spec = RunSpec::from_config(cfg, selinux);
    let script = egress_script(&cfg.boundary.gateway_container, cfg.gateway.forward_port);
    let cmd = RunnerCommand {
        argv: vec!["sh".into(), "-c".into(), script],
        ..Default::default()
    };
    // Clear a leftover of the same name from a crashed prior `--deep` run, or
    // this run fails with a name conflict instead of re-probing.
    let _ = podman(&["rm", "-f", "-i", "tracon-egress-probe"]).await;
    let args = spec.podman_args("tracon-egress-probe", &cmd, false);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    match podman(&argv).await {
        Ok(out) => egress_verdict(&out),
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
            "Mounts": [{"Source": "/state", "Destination": "/root/.omp"}],
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

    #[test]
    fn a_mounted_runtime_socket_fails() {
        let mut c = good_probe();
        c["Mounts"] = json!([{"Source": "/run/user/501/podman/podman.sock", "Destination": "/var/run/docker.sock"}]);
        let r = check_no_runtime_socket(&c);
        assert!(!r.ok);
        assert!(r.detail.contains("podman.sock"));
    }
}
