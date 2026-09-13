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

/// How long a denied connection may take to fail. A denial that *rejects* —
/// no route out, so `connect(2)` returns `ENETUNREACH` at once, and a name
/// with no reachable resolver fails the same way — is finished in
/// milliseconds. A denial that *drops* is indistinguishable from a slow
/// network and finishes only when the caller's own timeout does, which for
/// OpenCode is never: its LSP and formatter downloads pass no `AbortSignal`,
/// and the `edit` tool awaits them inline, so the first edit of a `.ts` file
/// blocks for as long as the network takes to say nothing
/// (`docs/reference/opencode-v1.18.30/config-state.md` §6.5). This bound is
/// what separates the two, and it is generous: the observed figure is under
/// 50 ms.
pub const REJECT_BOUND_SECS: u64 = 3;

/// The shell script the deep check runs inside the boundary. Six lines: direct
/// egress must fail, and must fail *fast* — by name and by address, because
/// resolution and connection are separate ways to hang — the allowlisted
/// provider must be reachable through the proxy, an unlisted host must be
/// refused, and the node must answer through the forward. Shared by every
/// backend so the proof is the same wherever it runs.
pub fn egress_script(gateway_host: &str, forward_port: u16) -> String {
    format!(
        "curl -s -o /dev/null -m 8 --noproxy '*' https://example.com && echo DIRECT_OK; \
         s=$(date +%s); \
         curl -s -o /dev/null -m 20 --noproxy '*' https://registry.npmjs.org/ && echo DIRECT_NAME_OK; \
         echo \"DIRECT_NAME_SECONDS=$(( $(date +%s) - s ))\"; \
         s=$(date +%s); \
         curl -s -o /dev/null -m 20 --noproxy '*' http://1.1.1.1/ && echo DIRECT_ADDR_OK; \
         echo \"DIRECT_ADDR_SECONDS=$(( $(date +%s) - s ))\"; \
         curl -s -o /dev/null -m 15 https://api.anthropic.com/ && echo PROXY_ALLOWED; \
         curl -s -o /dev/null -m 15 https://example.com/ && echo PROXY_UNLISTED; \
         curl -s -m 5 --noproxy '*' http://{gateway_host}:{forward_port}/harness/ping | head -c 40"
    )
}

/// What a backend claims its denial does, which decides whether a slow denial
/// is a failure or a recorded limitation.
///
/// Podman claims [`Denial::Rejects`]: its internal network has no route out, so
/// a denied connection fails at `connect(2)`. Kubernetes claims
/// [`Denial::Drops`]: a NetworkPolicy has no reject verb — dropping is what it
/// is — and no portable CNI option turns one into an RST. A node that cannot
/// reject still runs, because OpenCode is covered by the download flag and the
/// explicitly disabled server list; what it must not do is claim the property
/// it does not have, so the measurement is taken either way and the verdict
/// says which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denial {
    Rejects,
    Drops,
}

/// One `NAME=<seconds>` line from the script.
fn seconds(out: &str, key: &str) -> Option<u64> {
    out.lines().find_map(|line| {
        line.trim()
            .strip_prefix(key)?
            .strip_prefix('=')?
            .parse()
            .ok()
    })
}

/// Turn the script's output into the egress verdict.
pub fn egress_verdict(out: &str, denial: Denial) -> CheckResult {
    for marker in ["DIRECT_OK", "DIRECT_NAME_OK", "DIRECT_ADDR_OK"] {
        if out.contains(marker) {
            return CheckResult::fail(CheckId::Egress, "harness reached the internet directly");
        }
    }
    if out.contains("PROXY_UNLISTED") {
        return CheckResult::fail(CheckId::Egress, "gateway allowed an unlisted host");
    }
    let mut slowest = 0;
    for (key, what) in [
        ("DIRECT_NAME_SECONDS", "a denied hostname"),
        ("DIRECT_ADDR_SECONDS", "a denied address"),
    ] {
        match seconds(out, key) {
            Some(took) if took > REJECT_BOUND_SECS && denial == Denial::Rejects => {
                return CheckResult::fail(
                    CheckId::Egress,
                    format!(
                        "{what} took {took}s to fail, over the {REJECT_BOUND_SECS}s bound: \
                         this boundary drops rather than rejects, and a harness download \
                         with no timeout will hang on it"
                    ),
                )
            }
            Some(took) => slowest = slowest.max(took),
            None => {
                return CheckResult::fail(
                    CheckId::Egress,
                    format!("the probe did not report how long {what} took to fail"),
                )
            }
        }
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
    let denial = match denial {
        Denial::Rejects => format!("what is denied is refused within {slowest}s, not dropped"),
        Denial::Drops => format!(
            "what is denied is dropped, not refused (slowest denial {slowest}s): this runtime \
             cannot express a reject, so a harness whose downloads have no timeout of their own \
             is protected only by its configuration"
        ),
    };
    CheckResult::ok(
        CheckId::Egress,
        format!("no direct egress; {denial}; allowlisted host reachable, unlisted host refused"),
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

    /// A probe that failed fast on both denied destinations, as a rejecting
    /// boundary does.
    fn rejected() -> String {
        "DIRECT_NAME_SECONDS=0\nDIRECT_ADDR_SECONDS=0\n".to_string()
    }

    #[test]
    fn egress_verdict_reads_every_line() {
        let v = |out: &str| egress_verdict(out, Denial::Rejects);
        let ok = format!("{}PROXY_ALLOWED\npong", rejected());
        assert!(v(&ok).ok);
        assert!(!v(&format!("DIRECT_OK\n{ok}")).ok);
        assert!(!v(&format!("PROXY_UNLISTED\n{ok}")).ok);
        assert!(!v(&format!("{}PROXY_ALLOWED", rejected())).ok);
        assert!(!v(&format!("{}pong", rejected())).ok);
    }

    /// The point of the bound: a boundary that blackholes a denied
    /// destination passes every other line of this probe and still hangs the
    /// first edit of a TypeScript file, because OpenCode's download path has
    /// no timeout of its own.
    #[test]
    fn a_denial_that_drops_rather_than_rejects_fails_the_check() {
        let dropped = format!(
            "DIRECT_NAME_SECONDS={}\nDIRECT_ADDR_SECONDS=0\nPROXY_ALLOWED\npong",
            REJECT_BOUND_SECS + 17
        );
        let verdict = egress_verdict(&dropped, Denial::Rejects);
        assert!(!verdict.ok);
        assert!(verdict.detail.contains("drops rather than rejects"));
        // And the other direction, so neither destination is the only one
        // actually measured.
        let dropped = format!(
            "DIRECT_NAME_SECONDS=0\nDIRECT_ADDR_SECONDS={}\nPROXY_ALLOWED\npong",
            REJECT_BOUND_SECS + 17
        );
        assert!(!egress_verdict(&dropped, Denial::Rejects).ok);
    }

    /// A backend that cannot express a refusal is not failed for it — it is
    /// made to say so, on every check, rather than quietly reading as if it
    /// had the property Podman has.
    #[test]
    fn a_backend_that_only_drops_records_that_rather_than_failing() {
        let dropped = format!(
            "DIRECT_NAME_SECONDS={}\nDIRECT_ADDR_SECONDS=0\nPROXY_ALLOWED\npong",
            REJECT_BOUND_SECS + 17
        );
        let verdict = egress_verdict(&dropped, Denial::Drops);
        assert!(verdict.ok);
        assert!(verdict.detail.contains("dropped, not refused"));
        // Still a failure when something actually gets out.
        assert!(!egress_verdict(&format!("DIRECT_OK\n{dropped}"), Denial::Drops).ok);
    }

    /// A probe that reported no timing at all is not a pass: an unanswered
    /// question about the boundary is not a yes.
    #[test]
    fn a_probe_that_reported_no_timing_is_not_a_pass() {
        assert!(!egress_verdict("PROXY_ALLOWED\npong", Denial::Rejects).ok);
        assert!(!egress_verdict("PROXY_ALLOWED\npong", Denial::Drops).ok);
    }

    #[test]
    fn the_script_measures_both_a_name_and_an_address() {
        let script = egress_script("tracon-gw", 8890);
        assert!(script.contains("registry.npmjs.org"));
        assert!(script.contains("http://1.1.1.1/"));
        assert!(script.contains("DIRECT_NAME_SECONDS"));
        assert!(script.contains("DIRECT_ADDR_SECONDS"));
    }
}
