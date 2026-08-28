//! The vocabulary every backend answers in. Five questions the operator would
//! otherwise have to take on trust; each backend answers them from what it can
//! actually inspect, and the interface, `/api/nodes`, and `tracon
//! check-boundary` speak only this.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckId {
    Runtime,
    HarnessUnprivileged,
    NoRuntimeSocket,
    NetworkIsolated,
    Egress,
}

impl CheckId {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Runtime => "runtime",
            Self::HarnessUnprivileged => "harness_unprivileged",
            Self::NoRuntimeSocket => "no_runtime_socket",
            Self::NetworkIsolated => "network_isolated",
            Self::Egress => "egress",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    pub id: CheckId,
    pub ok: bool,
    pub detail: String,
}

impl CheckResult {
    pub fn ok(id: CheckId, detail: impl Into<String>) -> Self {
        Self {
            id,
            ok: true,
            detail: detail.into(),
        }
    }
    pub fn fail(id: CheckId, detail: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            detail: detail.into(),
        }
    }
}

/// The outcome of the startup verification.
#[derive(Debug, Clone, Serialize)]
pub struct BoundaryReport {
    pub checks: Vec<CheckResult>,
}

impl BoundaryReport {
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|c| c.ok)
    }

    pub fn first_failure(&self) -> Option<&CheckResult> {
        self.checks.iter().find(|c| !c.ok)
    }
}

/// The shell script the deep check runs inside the boundary. Four lines, one
/// verdict each: direct egress must fail, the allowlisted provider must be
/// reachable through the proxy, an unlisted host must be refused, and the
/// node must answer through the forward. Shared by every backend so the
/// proof is the same wherever it runs.
pub fn egress_script(gateway_host: &str, forward_port: u16) -> String {
    format!(
        "curl -s -o /dev/null -m 8 --noproxy '*' https://example.com && echo DIRECT_OK; \
         curl -s -o /dev/null -m 15 https://api.anthropic.com/ && echo PROXY_ALLOWED; \
         curl -s -o /dev/null -m 15 https://example.com/ && echo PROXY_UNLISTED; \
         curl -s -m 5 --noproxy '*' http://{gateway_host}:{forward_port}/harness/ping | head -c 40"
    )
}

/// Turn the script's output into the egress verdict.
pub fn egress_verdict(out: &str) -> CheckResult {
    if out.contains("DIRECT_OK") {
        return CheckResult::fail(CheckId::Egress, "harness reached the internet directly");
    }
    if out.contains("PROXY_UNLISTED") {
        return CheckResult::fail(CheckId::Egress, "gateway allowed an unlisted host");
    }
    if !out.contains("pong") {
        return CheckResult::fail(
            CheckId::Egress,
            "node not reachable through the gateway forward (is the node serving?)",
        );
    }
    if !out.contains("PROXY_ALLOWED") {
        return CheckResult::fail(
            CheckId::Egress,
            "harness could not reach an allowlisted provider through the gateway",
        );
    }
    CheckResult::ok(
        CheckId::Egress,
        "no direct egress; allowlisted host reachable, unlisted host refused",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_names_the_first_failure() {
        let report = BoundaryReport {
            checks: vec![
                CheckResult::ok(CheckId::Runtime, "fine"),
                CheckResult::fail(CheckId::NetworkIsolated, "not internal"),
            ],
        };
        assert!(!report.passed());
        assert_eq!(report.first_failure().unwrap().id, CheckId::NetworkIsolated);
    }

    #[test]
    fn egress_verdict_reads_every_line() {
        assert!(!egress_verdict("DIRECT_OK\nPROXY_ALLOWED\npong").ok);
        assert!(!egress_verdict("PROXY_ALLOWED\nPROXY_UNLISTED\npong").ok);
        assert!(!egress_verdict("PROXY_ALLOWED").ok);
        assert!(!egress_verdict("pong").ok);
        assert!(egress_verdict("PROXY_ALLOWED\npong").ok);
    }
}
