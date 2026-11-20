//! Where a session runs: one pure decision over the providers this node has
//! enabled, what each reports, and what the session is held to.
//!
//! Local providers are tried in the order given. DigitalOcean is the elastic
//! one, used per `[placement] digitalocean`: never when `off`; first when
//! `prefer`; and when `overflow`, automatically only once every local
//! provider that would take the session is full. A local boundary that is
//! not verified is never worked around on the node's own say: the operator
//! is asked whether to run on DigitalOcean instead.

use serde::Serialize;

use super::capabilities::Capabilities;
use super::kubernetes::room::Fit;
use super::requirements::{DoMode, Requirements};
use super::{Capacity, ProviderKind};

/// One enabled provider, as placement sees it.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub kind: ProviderKind,
    pub capabilities: Capabilities,
    pub capacity: Capacity,
    /// `Err` with the reason when its isolation is not verified (a boundary
    /// check failed): it is not run on, whatever it reports.
    pub health: Result<(), String>,
    /// Why it takes no new session now although it is healthy and has room:
    /// the node's `[spend] infra_usd_per_day` is reached.
    pub closed: Option<String>,
    /// What a cluster has room for, beside `capacity`'s count: a Kubernetes
    /// runtime is full when a session and one check would not fit, or when
    /// the scheduler has just refused this session's pod. `None` where the
    /// runtime has no such measure.
    pub room: Option<Room>,
}

/// A cluster's room for one more session, as placement holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Room {
    /// Read from the API (`kubernetes::room`).
    Read(Fit),
    /// Not read: the session count alone decides, as before.
    Unread { unread: String },
    /// The scheduler kept this session's pod unschedulable past its grace:
    /// full, whatever was read, in the scheduler's words.
    Unschedulable { fits: bool, unschedulable: String },
}

impl Room {
    pub fn unschedulable(said: impl Into<String>) -> Self {
        Room::Unschedulable {
            fits: false,
            unschedulable: said.into(),
        }
    }

    pub fn is_full(&self) -> bool {
        match self {
            Room::Read(fit) => !fit.fits,
            Room::Unread { .. } => false,
            Room::Unschedulable { .. } => true,
        }
    }
}

impl std::fmt::Display for Room {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Room::Read(fit) => fit.fmt(f),
            Room::Unread { unread } => write!(f, "the cluster's room was not read: {unread}"),
            Room::Unschedulable { unschedulable, .. } => write!(f, "its pod {unschedulable}"),
        }
    }
}

impl Candidate {
    /// Full by its count (`capacity`) or by its cluster's room.
    pub fn is_full(&self) -> bool {
        self.capacity.is_full() || self.room.as_ref().is_some_and(Room::is_full)
    }

    /// Why it is full, in the placement's words.
    fn full(&self) -> String {
        match &self.room {
            Some(room) if room.is_full() => format!("{} is full ({room})", self.kind.as_str()),
            _ => format!("{} is full ({})", self.kind.as_str(), self.capacity),
        }
    }
}

/// The decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Placement {
    /// Run there.
    Place { provider: ProviderKind },
    /// Run nowhere until the operator approves running on `offer`.
    Ask { reason: String, offer: ProviderKind },
    /// Run nowhere.
    Refuse { reason: String },
}

/// Why a local provider cannot take the session, in the order they are
/// asked: one that could never take it is not called full or unhealthy.
enum Local<'a> {
    Free(&'a Candidate),
    Full(&'a Candidate),
    Unhealthy(&'a Candidate, &'a str),
    Unsuitable(&'a Candidate, String),
}

fn local<'a>(c: &'a Candidate, requirements: &Requirements) -> Local<'a> {
    if let Err(why) = requirements.check(&c.capabilities) {
        return Local::Unsuitable(c, why);
    }
    if let Err(why) = &c.health {
        return Local::Unhealthy(c, why);
    }
    if c.is_full() {
        return Local::Full(c);
    }
    Local::Free(c)
}

/// Whether DigitalOcean could take the session now, or why not.
fn elastic<'a>(
    providers: &'a [Candidate],
    requirements: &Requirements,
    mode: DoMode,
    forbid: bool,
) -> Result<&'a Candidate, String> {
    if mode == DoMode::Off {
        return Err("DigitalOcean is off on this node".into());
    }
    if forbid {
        return Err("DigitalOcean is forbidden here".into());
    }
    let Some(c) = providers.iter().find(|c| c.kind.is_elastic()) else {
        return Err("DigitalOcean is not set up on this node".into());
    };
    if let Err(why) = requirements.check(&c.capabilities) {
        return Err(format!(
            "DigitalOcean does not meet the requirements: {why}"
        ));
    }
    if let Err(why) = &c.health {
        return Err(format!("DigitalOcean is not verified: {why}"));
    }
    if c.capacity.is_full() {
        return Err(format!("DigitalOcean is at its limit ({})", c.capacity));
    }
    if let Some(why) = &c.closed {
        return Err(format!("DigitalOcean takes no new session: {why}"));
    }
    Ok(c)
}

/// Decide where a session runs.
///
/// `providers` are the ones this node has enabled, locals in the order they
/// are preferred; `requirements` and `forbid_digitalocean` are what the
/// session's channel and repository resolved to
/// (`requirements::Placement::resolve`).
pub fn place(
    providers: &[Candidate],
    requirements: &Requirements,
    digitalocean: DoMode,
    forbid_digitalocean: bool,
) -> Placement {
    let elastic = elastic(providers, requirements, digitalocean, forbid_digitalocean);
    if digitalocean == DoMode::Prefer {
        if let Ok(c) = elastic {
            return Placement::Place { provider: c.kind };
        }
    }
    let locals: Vec<Local> = providers
        .iter()
        .filter(|c| !c.kind.is_elastic())
        .map(|c| local(c, requirements))
        .collect();

    if let Some(c) = locals.iter().find_map(|l| match l {
        Local::Free(c) => Some(*c),
        _ => None,
    }) {
        return Placement::Place { provider: c.kind };
    }

    let full: Vec<String> = locals
        .iter()
        .filter_map(|l| match l {
            Local::Full(c) => Some(c.full()),
            _ => None,
        })
        .collect();
    if !full.is_empty() {
        return match elastic {
            Ok(c) => Placement::Place { provider: c.kind },
            Err(why) => Placement::Refuse {
                reason: format!("{}; {why}", full.join("; ")),
            },
        };
    }

    let unhealthy: Vec<String> = locals
        .iter()
        .filter_map(|l| match l {
            Local::Unhealthy(c, why) => Some(format!(
                "{}'s boundary is not verified: {why}",
                c.kind.as_str()
            )),
            _ => None,
        })
        .collect();
    let unsuitable: Vec<String> = locals
        .iter()
        .filter_map(|l| match l {
            Local::Unsuitable(c, why) => Some(format!("{}: {why}", c.kind.as_str())),
            _ => None,
        })
        .collect();
    let reason = if !unhealthy.is_empty() {
        unhealthy.join("; ")
    } else if !unsuitable.is_empty() {
        format!(
            "no local provider meets the requirements ({})",
            unsuitable.join("; ")
        )
    } else {
        "no local provider is enabled".to_string()
    };
    match elastic {
        Ok(c) => Placement::Ask {
            reason,
            offer: c.kind,
        },
        Err(why) => Placement::Refuse {
            reason: format!("{reason}; {why}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::super::capabilities::{
        Dns, ImageIdentityKind, Privilege, Route, RuntimeSocket, Tier,
    };
    use super::*;

    fn container() -> Capabilities {
        Capabilities {
            tier: Tier::Container,
            route: Route::NodeOnly,
            dns: Dns::None,
            privilege: Privilege::Unprivileged,
            runtime_socket: RuntimeSocket::Absent,
            image_identity: ImageIdentityKind::Exact,
        }
    }

    fn droplet() -> Capabilities {
        Capabilities {
            tier: Tier::Microvm,
            route: Route::NodeOnly,
            dns: Dns::ProviderOpen,
            privilege: Privilege::RootInVm,
            runtime_socket: RuntimeSocket::Absent,
            image_identity: ImageIdentityKind::Derived,
        }
    }

    fn free(kind: ProviderKind) -> Candidate {
        Candidate {
            kind,
            capabilities: if kind.is_elastic() {
                droplet()
            } else {
                container()
            },
            capacity: Capacity {
                running: 1,
                limit: Some(4),
            },
            health: Ok(()),
            closed: None,
            room: None,
        }
    }

    fn full(kind: ProviderKind) -> Candidate {
        Candidate {
            capacity: Capacity {
                running: 4,
                limit: Some(4),
            },
            ..free(kind)
        }
    }

    fn unhealthy(kind: ProviderKind) -> Candidate {
        Candidate {
            health: Err("network_isolated failed".into()),
            ..free(kind)
        }
    }

    /// A provider whose route is unverified: it satisfies personal work and
    /// nothing that asks for `node-only`.
    fn unsuitable(kind: ProviderKind) -> Candidate {
        let mut c = free(kind);
        c.capabilities.route = Route::Unverified;
        c
    }

    fn node_only() -> Requirements {
        Requirements {
            route: Some(vec![Route::NodeOnly]),
            ..Requirements::default()
        }
    }

    use ProviderKind::{DigitalOcean as Do, Kubernetes, Podman};

    fn placed(p: &Placement) -> Option<ProviderKind> {
        match p {
            Placement::Place { provider } => Some(*provider),
            _ => None,
        }
    }

    fn asked(p: &Placement) -> bool {
        matches!(p, Placement::Ask { offer: Do, .. })
    }

    fn refused(p: &Placement) -> bool {
        matches!(p, Placement::Refuse { .. })
    }

    #[test]
    fn a_free_local_provider_takes_the_session() {
        for mode in [DoMode::Off, DoMode::Overflow] {
            let p = place(&[free(Podman), free(Do)], &Requirements::any(), mode, false);
            assert_eq!(placed(&p), Some(Podman), "{mode:?}");
        }
    }

    #[test]
    fn prefer_runs_on_digitalocean_first() {
        let p = place(
            &[free(Podman), free(Do)],
            &Requirements::any(),
            DoMode::Prefer,
            false,
        );
        assert_eq!(placed(&p), Some(Do));
    }

    #[test]
    fn prefer_falls_back_to_local_when_digitalocean_cannot_take_it() {
        for d in [unhealthy(Do), full(Do), unsuitable(Do)] {
            let p = place(&[free(Podman), d], &node_only(), DoMode::Prefer, false);
            assert_eq!(placed(&p), Some(Podman));
        }
        // Forbidden for the repository.
        let p = place(
            &[free(Podman), free(Do)],
            &Requirements::any(),
            DoMode::Prefer,
            true,
        );
        assert_eq!(placed(&p), Some(Podman));
    }

    #[test]
    fn overflow_is_automatic_only_at_capacity() {
        let p = place(
            &[full(Podman), free(Do)],
            &Requirements::any(),
            DoMode::Overflow,
            false,
        );
        assert_eq!(placed(&p), Some(Do));
    }

    #[test]
    fn a_reached_spend_limit_keeps_new_sessions_off_digitalocean() {
        let spent = || Candidate {
            closed: Some("today's infrastructure spend reached its limit".into()),
            ..free(Do)
        };
        // Prefer falls back to a local provider with room.
        let p = place(
            &[free(Podman), spent()],
            &Requirements::any(),
            DoMode::Prefer,
            false,
        );
        assert_eq!(placed(&p), Some(Podman));
        // Overflow at capacity is refused, and says why.
        let p = place(
            &[full(Podman), spent()],
            &Requirements::any(),
            DoMode::Overflow,
            false,
        );
        let Placement::Refuse { reason } = p else {
            panic!("{p:?}")
        };
        assert!(reason.contains("spend reached its limit"), "{reason}");
    }

    #[test]
    fn at_capacity_without_digitalocean_is_refused_and_says_why() {
        let p = place(&[full(Podman)], &Requirements::any(), DoMode::Off, false);
        let Placement::Refuse { reason } = p else {
            panic!("{p:?}")
        };
        assert!(reason.contains("podman is full (4 of 4)"), "{reason}");
        assert!(reason.contains("DigitalOcean is off"), "{reason}");

        let p = place(
            &[full(Podman), free(Do)],
            &Requirements::any(),
            DoMode::Overflow,
            true,
        );
        let Placement::Refuse { reason } = p else {
            panic!("{p:?}")
        };
        assert!(reason.contains("forbidden"), "{reason}");
    }

    #[test]
    fn an_unhealthy_local_boundary_asks_rather_than_overflowing() {
        let p = place(
            &[unhealthy(Podman), free(Do)],
            &Requirements::any(),
            DoMode::Overflow,
            false,
        );
        let Placement::Ask { reason, offer } = p else {
            panic!("{p:?}")
        };
        assert_eq!(offer, Do);
        assert!(
            reason.contains("podman's boundary is not verified"),
            "{reason}"
        );
        assert!(reason.contains("network_isolated failed"), "{reason}");
    }

    #[test]
    fn an_unhealthy_local_boundary_with_nothing_to_offer_is_refused() {
        for (mode, forbid) in [
            (DoMode::Off, false),
            (DoMode::Overflow, true),
            (DoMode::Prefer, true),
        ] {
            let p = place(
                &[unhealthy(Podman), free(Do)],
                &Requirements::any(),
                mode,
                forbid,
            );
            assert!(refused(&p), "{mode:?} {forbid}: {p:?}");
        }
        // DigitalOcean itself not verified.
        let p = place(
            &[unhealthy(Podman), unhealthy(Do)],
            &Requirements::any(),
            DoMode::Overflow,
            false,
        );
        let Placement::Refuse { reason } = p else {
            panic!("{p:?}")
        };
        assert!(reason.contains("DigitalOcean is not verified"), "{reason}");
    }

    #[test]
    fn client_work_is_never_offered_an_open_resolver() {
        let client = Requirements::client_work();
        // Healthy local: runs locally, even under prefer.
        let p = place(&[free(Podman), free(Do)], &client, DoMode::Prefer, false);
        assert_eq!(placed(&p), Some(Podman));
        // Full local: no overflow, since DigitalOcean's resolver is open.
        let p = place(&[full(Podman), free(Do)], &client, DoMode::Overflow, false);
        let Placement::Refuse { reason } = p else {
            panic!("{p:?}")
        };
        assert!(reason.contains("dns is provider-open"), "{reason}");
        // Unhealthy local: refused, not asked.
        let p = place(
            &[unhealthy(Podman), free(Do)],
            &client,
            DoMode::Overflow,
            false,
        );
        assert!(refused(&p), "{p:?}");
        // And the same node takes personal work there when asked.
        let p = place(
            &[unhealthy(Podman), free(Do)],
            &Requirements::any(),
            DoMode::Overflow,
            false,
        );
        assert!(asked(&p), "{p:?}");
    }

    #[test]
    fn a_local_provider_that_meets_no_requirement_is_never_placed() {
        let p = place(&[unsuitable(Podman)], &node_only(), DoMode::Off, false);
        let Placement::Refuse { reason } = p else {
            panic!("{p:?}")
        };
        assert!(
            reason.contains("no local provider meets the requirements"),
            "{reason}"
        );
        assert!(reason.contains("route is unverified"), "{reason}");
        // With DigitalOcean able to, the operator is asked, never placed.
        let p = place(
            &[unsuitable(Podman), free(Do)],
            &node_only(),
            DoMode::Overflow,
            false,
        );
        assert!(asked(&p), "{p:?}");
    }

    #[test]
    fn with_no_local_provider_overflow_asks_and_prefer_places() {
        let any = Requirements::any();
        let p = place(&[free(Do)], &any, DoMode::Overflow, false);
        let Placement::Ask { reason, .. } = &p else {
            panic!("{p:?}")
        };
        assert!(reason.contains("no local provider is enabled"));
        assert_eq!(
            placed(&place(&[free(Do)], &any, DoMode::Prefer, false)),
            Some(Do)
        );
        assert!(refused(&place(&[], &any, DoMode::Overflow, false)));
        assert!(refused(&place(&[], &any, DoMode::Off, false)));
    }

    #[test]
    fn locals_are_tried_in_order_and_past_one_that_cannot_take_it() {
        let any = Requirements::any();
        let p = place(&[free(Podman), free(Kubernetes)], &any, DoMode::Off, false);
        assert_eq!(placed(&p), Some(Podman));
        for first in [full(Podman), unhealthy(Podman)] {
            let p = place(&[first, free(Kubernetes)], &any, DoMode::Overflow, false);
            assert_eq!(placed(&p), Some(Kubernetes));
        }
        let p = place(
            &[unsuitable(Podman), free(Kubernetes)],
            &node_only(),
            DoMode::Off,
            false,
        );
        assert_eq!(placed(&p), Some(Kubernetes));
    }

    /// One healthy local full and another unhealthy: the session overflows,
    /// since the one that would take it is only busy.
    #[test]
    fn full_beside_unhealthy_overflows() {
        let p = place(
            &[unhealthy(Podman), full(Kubernetes), free(Do)],
            &Requirements::any(),
            DoMode::Overflow,
            false,
        );
        assert_eq!(placed(&p), Some(Do));
    }

    #[test]
    fn an_unlimited_capacity_is_never_full() {
        let mut c = free(Podman);
        c.capacity = Capacity {
            running: 10_000,
            limit: None,
        };
        assert_eq!(
            placed(&place(&[c], &Requirements::any(), DoMode::Off, false)),
            Some(Podman)
        );
        assert!(Capacity {
            running: 0,
            limit: Some(0)
        }
        .is_full());
    }

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum State {
        Free,
        Full,
        Unhealthy,
        Unsuitable,
        Absent,
    }

    fn make(kind: ProviderKind, s: State) -> Option<Candidate> {
        match s {
            State::Free => Some(free(kind)),
            State::Full => Some(full(kind)),
            State::Unhealthy => Some(unhealthy(kind)),
            State::Unsuitable => Some(unsuitable(kind)),
            State::Absent => None,
        }
    }

    /// Every combination of mode, forbid, one local and DigitalOcean, held
    /// to the rules as the plan states them.
    #[test]
    fn every_combination_follows_the_rules() {
        use State::*;
        let states = [Free, Full, Unhealthy, Unsuitable, Absent];
        for mode in [DoMode::Off, DoMode::Overflow, DoMode::Prefer] {
            for forbid in [false, true] {
                for l in states {
                    for d in states {
                        let providers: Vec<Candidate> = [make(Podman, l), make(Do, d)]
                            .into_iter()
                            .flatten()
                            .collect();
                        let p = place(&providers, &node_only(), mode, forbid);
                        let at = format!("{mode:?} forbid={forbid} local={l:?} do={d:?}: {p:?}");
                        let do_ok = mode != DoMode::Off && !forbid && d == Free;
                        let expected = if mode == DoMode::Prefer && do_ok {
                            "do"
                        } else {
                            match l {
                                Free => "local",
                                Full if do_ok => "do",
                                Full => "refuse",
                                _ if do_ok => "ask",
                                _ => "refuse",
                            }
                        };
                        let got = match &p {
                            Placement::Place { provider: Podman } => "local",
                            Placement::Place { provider: Do } => "do",
                            Placement::Ask { offer: Do, .. } => "ask",
                            Placement::Refuse { .. } => "refuse",
                            _ => "other",
                        };
                        assert_eq!(got, expected, "{at}");
                        // Invariants that hold whatever the rule table says.
                        if mode == DoMode::Off || forbid {
                            assert_ne!(placed(&p), Some(Do), "{at}");
                            assert!(!asked(&p), "{at}");
                        }
                        if l == Unhealthy && mode != DoMode::Prefer {
                            assert_ne!(placed(&p), Some(Do), "{at}");
                        }
                        match &p {
                            Placement::Refuse { reason } | Placement::Ask { reason, .. } => {
                                assert!(!reason.is_empty(), "{at}")
                            }
                            Placement::Place { .. } => {}
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_decision_serializes_for_an_approval_card() {
        let p = Placement::Ask {
            reason: "podman's boundary is not verified".into(),
            offer: Do,
        };
        assert_eq!(
            serde_json::to_value(&p).unwrap(),
            serde_json::json!({
                "decision": "ask",
                "reason": "podman's boundary is not verified",
                "offer": "digitalocean",
            })
        );
    }

    const GI: u64 = 1024 * 1024 * 1024;

    /// A Kubernetes runtime with no `local_limit` and `free` bytes free on
    /// its machine, against a session and its checks' 3 GiB.
    fn cluster(room: u64) -> Candidate {
        Candidate {
            capacity: Capacity {
                running: 4,
                limit: None,
            },
            room: Some(Room::Read(Fit {
                free_cpu: 2000,
                free_memory: room,
                needs_cpu: 750,
                needs_memory: 3 * GI,
                fits: room >= 3 * GI,
                awaits_node: false,
            })),
            ..free(Kubernetes)
        }
    }

    /// A cluster without room for a session and one check is full with no
    /// `local_limit`, under each `digitalocean` mode, and says why.
    #[test]
    fn a_cluster_without_room_is_full_under_each_mode() {
        let any = Requirements::any();
        let full = cluster(630 * 1024 * 1024);
        assert!(full.is_full());
        // Overflow and prefer move it.
        for mode in [DoMode::Overflow, DoMode::Prefer] {
            let p = place(&[full.clone(), free(Do)], &any, mode, false);
            assert_eq!(placed(&p), Some(Do), "{mode:?}");
        }
        // Off, or forbidden for the repository, refuses it with the figures.
        for (mode, forbid) in [(DoMode::Off, false), (DoMode::Overflow, true)] {
            let p = place(&[full.clone(), free(Do)], &any, mode, forbid);
            let Placement::Refuse { reason } = p else {
                panic!("{mode:?}: {p:?}")
            };
            assert!(
                reason.contains(
                    "kubernetes is full (the cluster has 630 Mi free, a session and its checks need 3 Gi)"
                ),
                "{reason}"
            );
        }
        // A session that fits stays, whatever the count.
        for mode in [DoMode::Off, DoMode::Overflow] {
            let p = place(&[cluster(4 * GI), free(Do)], &any, mode, false);
            assert_eq!(placed(&p), Some(Kubernetes), "{mode:?}");
        }
        // `local_limit` still caps it, whichever comes first.
        let mut capped = cluster(4 * GI);
        capped.capacity.limit = Some(4);
        let p = place(&[capped, free(Do)], &any, DoMode::Overflow, false);
        assert_eq!(placed(&p), Some(Do));
    }

    /// Room that was not read leaves the count alone to decide, as before;
    /// a pod the scheduler refused makes it full, in the scheduler's words.
    #[test]
    fn unread_room_counts_sessions_and_an_unschedulable_pod_is_full() {
        let any = Requirements::any();
        let unread = Candidate {
            room: Some(Room::Unread {
                unread: "the node may not read node general-1".into(),
            }),
            ..cluster(0)
        };
        assert!(!unread.is_full());
        let p = place(&[unread, free(Do)], &any, DoMode::Overflow, false);
        assert_eq!(placed(&p), Some(Kubernetes));

        let refused = Candidate {
            room: Some(Room::unschedulable(
                "could not be scheduled: 0/1 nodes are available: 1 Insufficient memory.",
            )),
            ..cluster(4 * GI)
        };
        let p = place(&[refused.clone(), free(Do)], &any, DoMode::Overflow, false);
        assert_eq!(placed(&p), Some(Do));
        let p = place(&[refused, free(Do)], &any, DoMode::Off, false);
        let Placement::Refuse { reason } = p else {
            panic!("{p:?}")
        };
        assert!(reason.contains("1 Insufficient memory"), "{reason}");
    }
}
