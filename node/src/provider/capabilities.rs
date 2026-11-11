//! What a sandbox provider can promise about the sandboxes it makes, in one
//! vocabulary every provider answers in, and how today's boundary checks
//! (`boundary::checks::CheckId`) read as those promises.
//!
//! A capability is reported, not enforced: placement (`super::placement`)
//! compares it with what a repository or channel requires, and the checks
//! interface shows it beside the check that established it.

use serde::{Deserialize, Serialize};

use crate::boundary::{BoundaryReport, CheckId};

use super::ProviderKind;

/// How the sandbox is separated from the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tier {
    /// A namespace on the host's kernel.
    Container,
    /// A kernel of its own.
    Microvm,
}

/// Where the sandbox's traffic can go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Route {
    /// Only to the node, and the provider showed it.
    NodeOnly,
    /// Not shown: anything the provider lets out may be reachable.
    Unverified,
}

/// What resolves names inside the sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Dns {
    /// No resolver at all.
    None,
    /// A resolver the node answers.
    Node,
    /// The provider's resolver, which answers any name and so carries data
    /// out in the names it is asked.
    ProviderOpen,
}

/// What the harness runs as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Privilege {
    /// No capabilities, no escalation. Only meaningful on a container, where
    /// root would be the host kernel's.
    Unprivileged,
    /// Root inside a VM's own kernel.
    RootInVm,
}

/// Whether a container runtime's socket is reachable from inside.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeSocket {
    Absent,
    Present,
}

/// Whether the sandbox runs the image the node named, byte for byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImageIdentityKind {
    /// The digest the node named.
    Exact,
    /// One the provider rebuilt from it, under a digest of its own.
    Derived,
}

/// What a provider promises about every sandbox it makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub tier: Tier,
    pub route: Route,
    pub dns: Dns,
    pub privilege: Privilege,
    pub runtime_socket: RuntimeSocket,
    pub image_identity: ImageIdentityKind,
}

/// One capability as the checks established it, for display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CapabilityLine {
    /// `tier`, `route`, `dns`, `privilege`, `runtime_socket`,
    /// `image_identity`.
    pub capability: &'static str,
    /// As the configuration spells it (`node-only`), or `None` where the
    /// check behind it did not pass and nothing can be claimed.
    pub value: Option<&'static str>,
    /// The check it rests on; `None` for what the provider's kind decides
    /// on its own.
    pub check: Option<CheckId>,
    pub detail: String,
}

/// A provider's capabilities as read from its boundary checks.
#[derive(Debug, Clone, Serialize)]
pub struct CapabilityReport {
    pub provider: ProviderKind,
    /// Every check passed: the provider is verified, and placement may use
    /// it.
    pub verified: bool,
    /// Every capability, established or not.
    pub lines: Vec<CapabilityLine>,
    /// The whole set, when every line has a value.
    pub capabilities: Option<Capabilities>,
    /// Whether placement holds sessions to these: a provider elsewhere's
    /// are read back from each sandbox and held to what its work requires;
    /// a local runtime's are shown beside its checks.
    pub enforced: bool,
    /// For a provider elsewhere (DigitalOcean): how its sandboxes reach the
    /// node, and whether it can take a session now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elsewhere: Option<Elsewhere>,
}

/// A provider elsewhere as the node sees it now.
#[derive(Debug, Clone, Serialize)]
pub struct Elsewhere {
    /// How its sandboxes reach the node: `address` (raw TLS to the node's
    /// public address), `hub relay` (raw TLS to the hub relay's listener),
    /// or `hub url` (the same TLS inside a WebSocket to the hub's own URL).
    pub reach: &'static str,
    /// What they dial.
    pub dial: String,
    /// The one destination each sandbox's egress admits, by address or name.
    pub egress: Vec<String>,
    /// When placement sends sessions there: `overflow` or `prefer`.
    pub placement: super::requirements::DoMode,
    /// Whether it can take a session now, and why not.
    pub healthy: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    pub capacity: super::Capacity,
    /// Each catalogue service, and whether it runs in a sandbox there.
    pub services: Vec<ServiceSupport>,
}

/// Whether one catalogue service runs on a provider, and why not.
#[derive(Debug, Clone, Serialize)]
pub struct ServiceSupport {
    pub name: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Container => "container",
            Self::Microvm => "microvm",
        }
    }
}

impl Route {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NodeOnly => "node-only",
            Self::Unverified => "unverified",
        }
    }
}

impl Dns {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Node => "node",
            Self::ProviderOpen => "provider-open",
        }
    }
}

impl Privilege {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unprivileged => "unprivileged",
            Self::RootInVm => "root-in-vm",
        }
    }
}

impl RuntimeSocket {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Present => "present",
        }
    }
}

impl ImageIdentityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Derived => "derived",
        }
    }
}

/// Read a Podman or Kubernetes boundary report as capabilities.
///
/// Both run harnesses as containers, from images pinned by digest, so tier
/// and image identity follow from the kind once the runtime answers. The
/// rest each rest on one check: `harness_unprivileged` for privilege,
/// `no_runtime_socket` for the socket, `network_isolated` (and `egress`,
/// when a deep check ran it) for route and DNS. A check that did not pass
/// claims nothing, except route, whose own word for that is `unverified`.
pub fn from_checks(provider: ProviderKind, report: &BoundaryReport) -> CapabilityReport {
    let find = |id: CheckId| report.checks.iter().find(|c| c.id == id);
    let passed = |id: CheckId| find(id).is_some_and(|c| c.ok);
    // The check's own words: a line with no value already says nothing is
    // claimed.
    let why = |id: CheckId| {
        find(id).map_or_else(|| "the check did not run".to_string(), |c| c.detail.clone())
    };
    let runtime = passed(CheckId::Runtime);
    let kind_line = |capability, value: &'static str, what: &str| CapabilityLine {
        capability,
        value: runtime.then_some(value),
        check: Some(CheckId::Runtime),
        detail: if runtime {
            format!("{} {what}", provider.as_str())
        } else {
            why(CheckId::Runtime)
        },
    };
    let held = |capability, id: CheckId, value: &'static str| CapabilityLine {
        capability,
        value: passed(id).then_some(value),
        check: Some(id),
        detail: why(id),
    };

    // A deep check that ran and failed overrides the network check: it is
    // the stronger proof, taken from inside.
    let egress = find(CheckId::Egress);
    let route_check = match egress {
        Some(c) if !c.ok => CheckId::Egress,
        _ => CheckId::NetworkIsolated,
    };
    let route_ok = passed(CheckId::NetworkIsolated) && egress.is_none_or(|c| c.ok);
    let route = CapabilityLine {
        capability: "route",
        value: Some(if route_ok {
            Route::NodeOnly.as_str()
        } else {
            Route::Unverified.as_str()
        }),
        check: Some(route_check),
        detail: match egress {
            Some(c) if c.ok && route_ok => {
                format!("{}; {}", why(CheckId::NetworkIsolated), c.detail)
            }
            _ => why(route_check),
        },
    };

    // A Mac's sandboxes are each a VM, whose root is the VM's own.
    let (tier, tier_what, privilege) = match provider {
        ProviderKind::Apple => (
            Tier::Microvm,
            "runs each harness in a VM of its own",
            Privilege::RootInVm,
        ),
        _ => (
            Tier::Container,
            "runs each harness as a container",
            Privilege::Unprivileged,
        ),
    };
    let lines = vec![
        kind_line("tier", tier.as_str(), tier_what),
        route,
        held("dns", CheckId::NetworkIsolated, Dns::None.as_str()),
        held(
            "privilege",
            CheckId::HarnessUnprivileged,
            privilege.as_str(),
        ),
        held(
            "runtime_socket",
            CheckId::NoRuntimeSocket,
            RuntimeSocket::Absent.as_str(),
        ),
        kind_line(
            "image_identity",
            ImageIdentityKind::Exact.as_str(),
            "runs the image the node pinned by digest",
        ),
    ];
    let capabilities = lines
        .iter()
        .all(|line| line.value.is_some())
        .then_some(Capabilities {
            tier,
            route: if route_ok {
                Route::NodeOnly
            } else {
                Route::Unverified
            },
            dns: Dns::None,
            privilege,
            runtime_socket: RuntimeSocket::Absent,
            image_identity: ImageIdentityKind::Exact,
        });
    CapabilityReport {
        provider,
        verified: report.passed(),
        lines,
        capabilities,
        enforced: false,
        elsewhere: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boundary::CheckResult;

    fn all_ok() -> BoundaryReport {
        BoundaryReport {
            checks: vec![
                CheckResult::ok(CheckId::Runtime, "podman 5, rootless"),
                CheckResult::ok(CheckId::HarnessUnprivileged, "unprivileged"),
                CheckResult::ok(CheckId::NoRuntimeSocket, "no socket"),
                CheckResult::ok(CheckId::NetworkIsolated, "internal, no DNS"),
            ],
        }
    }

    fn with(mut report: BoundaryReport, failed: CheckId) -> BoundaryReport {
        for c in &mut report.checks {
            if c.id == failed {
                *c = CheckResult::fail(failed, "broken");
            }
        }
        report
    }

    fn value(report: &CapabilityReport, capability: &str) -> Option<&'static str> {
        report
            .lines
            .iter()
            .find(|l| l.capability == capability)
            .unwrap()
            .value
    }

    #[test]
    fn a_macs_checks_read_as_a_vm() {
        let r = from_checks(ProviderKind::Apple, &all_ok());
        let caps = r.capabilities.unwrap();
        assert_eq!(caps.tier, Tier::Microvm);
        assert_eq!(caps.privilege, Privilege::RootInVm);
        assert_eq!(caps.route, Route::NodeOnly);
    }

    #[test]
    fn passing_checks_read_as_the_full_set() {
        for kind in [ProviderKind::Podman, ProviderKind::Kubernetes] {
            let r = from_checks(kind, &all_ok());
            assert!(r.verified);
            assert!(!r.enforced);
            assert_eq!(
                r.capabilities,
                Some(Capabilities {
                    tier: Tier::Container,
                    route: Route::NodeOnly,
                    dns: Dns::None,
                    privilege: Privilege::Unprivileged,
                    runtime_socket: RuntimeSocket::Absent,
                    image_identity: ImageIdentityKind::Exact,
                })
            );
            let names: Vec<_> = r.lines.iter().map(|l| l.capability).collect();
            assert_eq!(
                names,
                [
                    "tier",
                    "route",
                    "dns",
                    "privilege",
                    "runtime_socket",
                    "image_identity"
                ]
            );
        }
    }

    #[test]
    fn each_check_maps_onto_its_own_capability() {
        let cases = [
            (CheckId::HarnessUnprivileged, "privilege"),
            (CheckId::NoRuntimeSocket, "runtime_socket"),
            (CheckId::NetworkIsolated, "dns"),
        ];
        for (check, capability) in cases {
            let r = from_checks(ProviderKind::Podman, &with(all_ok(), check));
            assert!(!r.verified);
            assert_eq!(r.capabilities, None, "{check:?}");
            assert_eq!(value(&r, capability), None, "{check:?}");
            let line = r.lines.iter().find(|l| l.capability == capability).unwrap();
            assert_eq!(line.check, Some(check));
            assert_eq!(line.detail, "broken");
            // Nothing else is taken down with it.
            for other in ["tier", "image_identity"] {
                assert!(value(&r, other).is_some(), "{check:?} took {other}");
            }
        }
    }

    #[test]
    fn a_failed_network_check_leaves_the_route_unverified() {
        let r = from_checks(
            ProviderKind::Kubernetes,
            &with(all_ok(), CheckId::NetworkIsolated),
        );
        assert_eq!(value(&r, "route"), Some("unverified"));
        let route = r.lines.iter().find(|l| l.capability == "route").unwrap();
        assert_eq!(route.detail, "broken");
        assert_eq!(value(&r, "dns"), None);
    }

    #[test]
    fn a_failed_runtime_claims_nothing_the_kind_would_have() {
        let r = from_checks(ProviderKind::Podman, &with(all_ok(), CheckId::Runtime));
        assert_eq!(value(&r, "tier"), None);
        assert_eq!(value(&r, "image_identity"), None);
        assert!(!r.verified);
    }

    #[test]
    fn a_deep_egress_check_decides_the_route_when_it_ran() {
        let mut deep = all_ok();
        deep.checks
            .push(CheckResult::ok(CheckId::Egress, "no direct egress"));
        let r = from_checks(ProviderKind::Podman, &deep);
        assert_eq!(value(&r, "route"), Some("node-only"));
        let route = r.lines.iter().find(|l| l.capability == "route").unwrap();
        assert!(route.detail.contains("no direct egress"));

        let failed = with(deep, CheckId::Egress);
        let r = from_checks(ProviderKind::Podman, &failed);
        assert_eq!(value(&r, "route"), Some("unverified"));
        let route = r.lines.iter().find(|l| l.capability == "route").unwrap();
        assert_eq!(route.check, Some(CheckId::Egress));
        // DNS rests on the network check, which still passed.
        assert_eq!(value(&r, "dns"), Some("none"));
        assert_eq!(r.capabilities.unwrap().route, Route::Unverified);
    }

    #[test]
    fn a_check_that_never_ran_is_not_a_pass() {
        let r = from_checks(
            ProviderKind::Podman,
            &BoundaryReport {
                checks: vec![CheckResult::ok(CheckId::Runtime, "fine")],
            },
        );
        assert_eq!(value(&r, "privilege"), None);
        assert!(r
            .lines
            .iter()
            .find(|l| l.capability == "privilege")
            .unwrap()
            .detail
            .contains("did not run"));
        assert_eq!(value(&r, "route"), Some("unverified"));
    }

    fn label<T: Serialize>(value: T) -> String {
        serde_json::to_value(value)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn values_are_spelled_as_the_configuration_spells_them() {
        assert_eq!(label(Route::NodeOnly), Route::NodeOnly.as_str());
        assert_eq!(label(Dns::ProviderOpen), Dns::ProviderOpen.as_str());
        assert_eq!(label(Privilege::RootInVm), Privilege::RootInVm.as_str());
        assert_eq!(label(Tier::Microvm), Tier::Microvm.as_str());
        assert_eq!(
            label(RuntimeSocket::Present),
            RuntimeSocket::Present.as_str()
        );
        assert_eq!(
            label(ImageIdentityKind::Derived),
            ImageIdentityKind::Derived.as_str()
        );
    }
}
