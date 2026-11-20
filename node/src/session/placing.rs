//! Where a starting session runs: the node's own runtime, or DigitalOcean's
//! Harness Runtime (`[placement]`, `crate::provider::placement`).
//!
//! Placement is in play only where it can change something: DigitalOcean
//! is on here, or the session's channel or repository requires more of a
//! provider than any verified one gives. Otherwise a session starts where
//! every session started before, and nothing is recorded.
//!
//! What the decision means:
//! - **Place**: run there. DigitalOcean is chosen on the node's own say only
//!   when every local provider that would take the session is full, or with
//!   `prefer`.
//! - **Ask**: this node's boundary is not verified (or cannot take the
//!   session) and DigitalOcean could. The operator gets a card; allowing it
//!   runs the session there, anything else ends it.
//! - **Refuse**: nowhere can take it; the session ends with why.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::provider::digitalocean::runtime::DoRuntime;
use crate::provider::requirements::{DoMode, Resolved};
use crate::provider::{Candidate, Capacity, Placement, ProviderKind, Requirements};
use crate::store::approvals::{ApprovalRow, PENDING, PLACE_TOOL};
use crate::store::{NodeRow, Store};

/// Where the session runs.
pub enum Placed {
    Local,
    DigitalOcean {
        runtime: Arc<DoRuntime>,
        requirements: Requirements,
    },
}

impl Placed {
    pub fn is_elsewhere(&self) -> bool {
        matches!(self, Placed::DigitalOcean { .. })
    }
}

/// How often a session waiting on its placement card looks at it.
pub const ASK_POLL: Duration = Duration::from_secs(1);

/// Whether placement decides anything for this session. A memory floor
/// alone does not: it never refuses a provider, and only DigitalOcean reads
/// it.
pub fn in_play(digitalocean: bool, resolved: &Resolved) -> bool {
    digitalocean || resolved.requirements.constrains_capabilities()
}

/// The node's own runtime as placement sees it: what its verified boundary
/// gives (a VM of its own per sandbox on a Mac, a container elsewhere),
/// healthy while the node's checks pass, full at `[placement] local_limit`.
pub fn local_candidate(
    kind: ProviderKind,
    health: Result<(), String>,
    running: u32,
    limit: Option<u32>,
) -> Candidate {
    use crate::provider::capabilities::{
        Capabilities, Dns, ImageIdentityKind, Privilege, Route, RuntimeSocket, Tier,
    };
    let (tier, privilege) = match kind {
        ProviderKind::Apple => (Tier::Microvm, Privilege::RootInVm),
        _ => (Tier::Container, Privilege::Unprivileged),
    };
    Candidate {
        kind,
        capabilities: Capabilities {
            tier,
            route: Route::NodeOnly,
            dns: Dns::None,
            privilege,
            runtime_socket: RuntimeSocket::Absent,
            image_identity: ImageIdentityKind::Exact,
        },
        capacity: Capacity { running, limit },
        health,
        closed: None,
        room: None,
    }
}

/// Whether the node's own boundary is verified, as its last checks found.
pub fn node_health(node: Option<&NodeRow>) -> Result<(), String> {
    match node {
        Some(node) if node.state != "ready" => Err(node
            .failed_detail
            .clone()
            .or_else(|| node.failed_check.clone())
            .unwrap_or_else(|| "a boundary check failed".into())),
        _ => Ok(()),
    }
}

/// What a `placement` event says: the decision, and what it was held to.
pub fn event(decision: &Placement, resolved: &Resolved, providers: &[Candidate]) -> Value {
    let mut payload = serde_json::to_value(decision).unwrap_or_else(|_| json!({}));
    payload["client"] = json!(resolved.client);
    payload["forbid_digitalocean"] = json!(resolved.forbid_digitalocean);
    payload["requirements"] = serde_json::to_value(&resolved.requirements).unwrap_or_default();
    payload["providers"] = providers
        .iter()
        .map(|c| {
            // A cluster's room beside its count: free and needed figures,
            // or why they were not read.
            let mut capacity = json!(c.capacity);
            if let (Some(room), Some(fields)) = (
                c.room.as_ref().and_then(|r| serde_json::to_value(r).ok()),
                capacity.as_object_mut(),
            ) {
                fields.extend(room.as_object().cloned().unwrap_or_default());
            }
            json!({
                "provider": c.kind,
                "capacity": capacity,
                "healthy": c.health.is_ok(),
                "why": c.health.as_ref().err(),
            })
        })
        .collect();
    payload
}

/// Where a review's required checks run.
pub enum ChecksPlaced {
    /// On the node's own runtime, in its line when that is short of room.
    Local,
    /// Forked on DigitalOcean (`review::forked`).
    DigitalOcean {
        runtime: Arc<DoRuntime>,
        requirements: Requirements,
    },
}

/// The node's own runtime as a review's checks see it: what its verified
/// boundary gives, held to no count (checks never counted against
/// `local_limit`), and full only when `short` says why: its checks were
/// put out or refused for want of room, or others' wait for it.
pub fn checks_candidate(
    kind: ProviderKind,
    health: Result<(), String>,
    short: Option<&str>,
) -> Candidate {
    let mut local = local_candidate(kind, health, 0, None);
    local.room = short.map(crate::provider::Room::short);
    local
}

/// Where a review's checks run, from what the session's channel and
/// repository resolved to: placed as a session is (`provider::place`), and
/// elsewhere only when that places them on DigitalOcean. A client's are
/// never placed there, whatever else allows it. Asking the operator is for
/// sessions: checks on a node whose boundary is not verified run where
/// they always have, and checks nowhere else can take wait in the node's
/// line.
pub fn place_checks(providers: &[Candidate], resolved: &Resolved, mode: DoMode) -> Placement {
    crate::provider::place(
        providers,
        &resolved.requirements,
        mode,
        resolved.forbid_digitalocean || resolved.client,
    )
}

/// Whether `decision` sends a review's checks to DigitalOcean.
pub fn checks_elsewhere(decision: &Placement) -> bool {
    matches!(
        decision,
        Placement::Place {
            provider: ProviderKind::DigitalOcean
        }
    )
}

/// One card per session.
fn ask_key(session_id: &str) -> String {
    crate::corpus::hash_body(&format!("{PLACE_TOOL}\u{1f}{session_id}"))
}

/// Find or raise the card asking the operator to run `session_id` on
/// `offer`, because of `reason`.
pub fn ask(
    store: &Store,
    node_id: &str,
    channel: &str,
    session_id: &str,
    reason: &str,
    offer: ProviderKind,
    expiry_secs: u64,
) -> Result<ApprovalRow, crate::store::StoreError> {
    let key = ask_key(session_id);
    let now = crate::store::now_ms();
    if let Some(open) = store.latest_approval(channel, &key)? {
        if open.state == PENDING && open.expires_ms > now {
            return Ok(open);
        }
    }
    let row = ApprovalRow {
        id: uuid::Uuid::now_v7().to_string(),
        channel: channel.to_string(),
        session_id: Some(session_id.to_string()),
        node_id: node_id.to_string(),
        lane: None,
        tool: PLACE_TOOL.to_string(),
        arguments: json!({
            "session": session_id,
            "provider": offer,
            "reason": reason,
        })
        .to_string(),
        request_key: key,
        title: format!("run this session on DigitalOcean: {reason}"),
        state: PENDING.into(),
        answer_option_id: None,
        edited_arguments: None,
        result: None,
        reason: None,
        operator_note: None,
        claimed_ms: None,
        created_ms: now,
        decided_ms: None,
        finished_ms: None,
        expires_ms: now + (expiry_secs.max(1) as i64) * 1000,
    };
    store.insert_approval(&row)?;
    Ok(row)
}

/// Where a placement card stands.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    Waiting,
    Allowed,
    /// Refused, expired or gone, with why.
    Declined(String),
}

pub fn answer(store: &Store, id: &str) -> Result<Answer, crate::store::StoreError> {
    use crate::store::approvals as a;
    let Some(row) = store.get_approval(id)? else {
        return Ok(Answer::Declined("its card is gone".into()));
    };
    Ok(match row.state.as_str() {
        a::PENDING if row.expires_ms > crate::store::now_ms() => Answer::Waiting,
        a::PENDING | a::EXPIRED => {
            Answer::Declined("nobody answered whether to run it on DigitalOcean".into())
        }
        a::RUNNING | a::SUCCEEDED
            if row.answer_option_id.as_deref()
                == Some(crate::adapter::types::OPTION_ALLOW_ONCE) =>
        {
            Answer::Allowed
        }
        _ => Answer::Declined(
            row.reason
                .unwrap_or_else(|| "the operator did not allow running it on DigitalOcean".into()),
        ),
    })
}

/// What placement resolves a session to, from its channel and the
/// repository its work came from.
pub fn resolve(cfg: &crate::config::Config, channel: &str, origin: Option<&Path>) -> Resolved {
    cfg.live().placement.resolve(channel, origin)
}

/// What counts against `[placement] local_limit`: the live sessions on the
/// node's own runtime, and the ones placed there and still starting. A start
/// takes minutes between its placement and its harness being up (the
/// workspace, an image pull), and a burst of starts that only counted live
/// sessions would each see the same free slots.
#[derive(Default)]
pub struct Slots {
    /// Held while one start decides where it runs and takes its slot, so two
    /// starts cannot both see the same one free.
    deciding: tokio::sync::Mutex<()>,
    held: parking_lot::Mutex<Held>,
}

#[derive(Default)]
struct Held {
    /// Sessions in `start`: `true` once placed on the node's own runtime.
    starting: HashMap<String, bool>,
    /// Sessions placed off this node's runtime (on DigitalOcean), starting
    /// or live.
    elsewhere: HashSet<String>,
}

/// A session in `start`, held until it returns: by then the session is live,
/// or it never will be and its slot is free again.
pub struct Starting {
    slots: Arc<Slots>,
    id: String,
}

impl Drop for Starting {
    fn drop(&mut self) {
        self.slots.held.lock().starting.remove(&self.id);
    }
}

impl Slots {
    /// `id` is starting, until the guard is dropped.
    pub fn starting(self: &Arc<Self>, id: &str) -> Starting {
        self.held.lock().starting.insert(id.to_string(), false);
        Starting {
            slots: self.clone(),
            id: id.to_string(),
        }
    }

    /// Held from counting the sessions here to taking a slot.
    pub async fn deciding(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.deciding.lock().await
    }

    /// `id` runs on the node's own runtime: it holds a slot from now on.
    pub fn local(&self, id: &str) {
        if let Some(local) = self.held.lock().starting.get_mut(id) {
            *local = true;
        }
    }

    /// `id` runs elsewhere: it holds none, and gives back the slot it held
    /// here if it was placed here first (its pod could not be scheduled).
    pub fn elsewhere(&self, id: &str) {
        let mut held = self.held.lock();
        if let Some(local) = held.starting.get_mut(id) {
            *local = false;
        }
        held.elsewhere.insert(id.to_string());
    }

    /// Sessions on the node's own runtime now, besides `id`: the `live`
    /// ones not placed elsewhere, and the ones placed here still starting.
    pub fn running_locally(&self, live: &[String], id: &str) -> u32 {
        let mut held = self.held.lock();
        let Held {
            starting,
            elsewhere,
        } = &mut *held;
        elsewhere.retain(|other| live.contains(other) || starting.contains_key(other));
        let mut here: HashSet<&String> = live
            .iter()
            .filter(|other| !elsewhere.contains(*other))
            .collect();
        here.extend(
            starting
                .iter()
                .filter(|(_, local)| **local)
                .map(|(other, _)| other),
        );
        here.into_iter().filter(|other| *other != id).count() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::digitalocean;

    fn node(state: &str) -> Result<(), String> {
        match state {
            "ready" => Ok(()),
            _ => Err("the gateway is not running".into()),
        }
    }

    fn droplet(running: u32) -> Candidate {
        Candidate {
            kind: ProviderKind::DigitalOcean,
            capabilities: digitalocean::capabilities(),
            capacity: Capacity {
                running,
                limit: Some(4),
            },
            health: Ok(()),
            closed: None,
            room: None,
        }
    }

    fn placement(text: &str) -> crate::provider::requirements::Placement {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn placement_is_out_of_play_by_default() {
        let resolved = placement("").resolve("personal", Some(Path::new("/src/blog")));
        assert!(!in_play(false, &resolved));
        assert!(in_play(true, &resolved));
        let client = placement("[[repo]]\npath = \"/src/hounddogreading\"\nclient = true\n")
            .resolve("personal", Some(Path::new("/src/hounddogreading")));
        assert!(in_play(false, &client));
        // A memory floor alone, with DigitalOcean off, changes nothing here.
        let floor = placement("[[repo]]\npath = \"/src/big\"\n[repo.require]\nmemory = \"4Gi\"\n")
            .resolve("personal", Some(Path::new("/src/big")));
        assert_eq!(floor.requirements.memory.as_deref(), Some("4Gi"));
        assert!(!in_play(false, &floor));
        assert!(in_play(true, &floor));
    }

    #[test]
    fn a_full_node_overflows_and_a_free_one_does_not() {
        let any = Requirements::any();
        let full = local_candidate(ProviderKind::Podman, node("ready"), 2, Some(2));
        let p = crate::provider::place(&[full, droplet(0)], &any, DoMode::Overflow, false);
        assert_eq!(
            p,
            Placement::Place {
                provider: ProviderKind::DigitalOcean
            }
        );
        let free = local_candidate(ProviderKind::Podman, node("ready"), 1, Some(2));
        let p = crate::provider::place(&[free, droplet(0)], &any, DoMode::Overflow, false);
        assert_eq!(
            p,
            Placement::Place {
                provider: ProviderKind::Podman
            }
        );
    }

    #[test]
    fn an_unverified_node_asks_rather_than_moves() {
        let sick = local_candidate(ProviderKind::Podman, node("refused"), 0, None);
        let p = crate::provider::place(
            &[sick, droplet(0)],
            &Requirements::any(),
            DoMode::Overflow,
            false,
        );
        let Placement::Ask { reason, offer } = p else {
            panic!("{p:?}")
        };
        assert_eq!(offer, ProviderKind::DigitalOcean);
        assert!(reason.contains("the gateway is not running"), "{reason}");
    }

    #[test]
    fn client_work_never_reaches_digitalocean() {
        let resolved = placement(
            "digitalocean = \"prefer\"\n[[repo]]\npath = \"/src/hounddogreading\"\nclient = true\n",
        )
        .resolve("personal", Some(Path::new("/src/hounddogreading")));
        // Full here, and DigitalOcean's resolver is open: refused, not moved.
        let full = local_candidate(ProviderKind::Podman, node("ready"), 2, Some(2));
        let p = crate::provider::place(
            &[full, droplet(0)],
            &resolved.requirements,
            DoMode::Prefer,
            resolved.forbid_digitalocean,
        );
        let Placement::Refuse { reason } = p else {
            panic!("{p:?}")
        };
        assert!(reason.contains("dns is provider-open"), "{reason}");
        // And an unverified node does not offer it either.
        let sick = local_candidate(ProviderKind::Podman, node("refused"), 0, None);
        let p = crate::provider::place(
            &[sick, droplet(0)],
            &resolved.requirements,
            DoMode::Overflow,
            false,
        );
        assert!(matches!(p, Placement::Refuse { .. }), "{p:?}");
    }

    /// A Mac's own sandboxes are VMs: work that requires one stays on the
    /// node, where a container node would have nowhere to put it.
    #[test]
    fn a_mac_takes_work_that_requires_a_vm() {
        let mut vm = Requirements::any();
        vm.tier = Some(vec![crate::provider::capabilities::Tier::Microvm]);
        let mac = local_candidate(ProviderKind::Apple, node("ready"), 0, None);
        assert_eq!(
            mac.capabilities.tier,
            crate::provider::capabilities::Tier::Microvm
        );
        let p = crate::provider::place(&[mac], &vm, DoMode::Off, false);
        assert_eq!(
            p,
            Placement::Place {
                provider: ProviderKind::Apple
            }
        );
        let linux = local_candidate(ProviderKind::Podman, node("ready"), 0, None);
        let p = crate::provider::place(&[linux], &vm, DoMode::Off, false);
        assert!(matches!(p, Placement::Refuse { .. }), "{p:?}");
    }

    /// A review's checks short of room here go to DigitalOcean when their
    /// repository is not a client's; a client's stay on the node, in its
    /// line, whatever `[placement]` allows; and checks with room here stay.
    #[test]
    fn checks_short_of_room_overflow_unless_they_are_a_clients() {
        let open = placement("digitalocean = \"overflow\"\n[[repo]]\npath = \"/src/hounddogreading\"\nclient = true\n");
        let personal = open.resolve("personal", Some(Path::new("/src/tracon")));
        let client = open.resolve("personal", Some(Path::new("/src/hounddogreading")));
        let short = || {
            checks_candidate(
                ProviderKind::Kubernetes,
                node("ready"),
                Some("its pod was evicted"),
            )
        };
        let roomy = || checks_candidate(ProviderKind::Kubernetes, node("ready"), None);

        let p = place_checks(&[short(), droplet(0)], &personal, DoMode::Overflow);
        assert!(checks_elsewhere(&p), "{p:?}");
        let p = place_checks(&[roomy(), droplet(0)], &personal, DoMode::Overflow);
        assert!(!checks_elsewhere(&p), "{p:?}");
        // DigitalOcean off: short checks wait in the node's line.
        let p = place_checks(&[short(), droplet(0)], &personal, DoMode::Off);
        assert!(!checks_elsewhere(&p), "{p:?}");

        for mode in [DoMode::Overflow, DoMode::Prefer] {
            let p = place_checks(&[short(), droplet(0)], &client, mode);
            assert!(!checks_elsewhere(&p), "{mode:?} {p:?}");
            let p = place_checks(&[roomy(), droplet(0)], &client, mode);
            assert!(!checks_elsewhere(&p), "{mode:?} {p:?}");
        }
        // A client entry whose requirements DigitalOcean happened to meet
        // is still kept off it.
        let mut lenient = client.clone();
        lenient.requirements = Requirements::any();
        let p = place_checks(&[short(), droplet(0)], &lenient, DoMode::Overflow);
        assert!(!checks_elsewhere(&p), "{p:?}");
        // Prefer sends a personal repository's checks there with room here.
        let p = place_checks(&[roomy(), droplet(0)], &personal, DoMode::Prefer);
        assert!(checks_elsewhere(&p), "{p:?}");
        // What the event records of the node's runtime: short, and why.
        let e = event(&p, &personal, &[short()]);
        assert_eq!(
            e["providers"][0]["capacity"]["short"],
            "its pod was evicted"
        );
        // Checks are never asked about: an unverified node runs them there.
        let sick = checks_candidate(ProviderKind::Podman, node("refused"), None);
        let p = place_checks(&[sick, droplet(0)], &personal, DoMode::Overflow);
        assert!(matches!(p, Placement::Ask { .. }), "{p:?}");
        assert!(!checks_elsewhere(&p));
    }

    #[test]
    fn the_event_says_what_was_decided_and_why() {
        let resolved = placement("").resolve("personal", None);
        let providers = [
            local_candidate(ProviderKind::Podman, node("refused"), 0, None),
            droplet(1),
        ];
        let decision = Placement::Place {
            provider: ProviderKind::DigitalOcean,
        };
        let e = event(&decision, &resolved, &providers);
        assert_eq!(e["decision"], "place");
        assert_eq!(e["provider"], "digitalocean");
        assert_eq!(e["providers"][0]["healthy"], false);
        assert_eq!(e["providers"][1]["capacity"]["running"], 1);
    }

    /// A Kubernetes runtime's capacity carries its cluster's room: the free
    /// and needed figures, or why they were not read.
    #[test]
    fn the_event_carries_the_clusters_room() {
        use crate::provider::kubernetes::room::Fit;
        use crate::provider::Room;
        let resolved = placement("").resolve("personal", None);
        let mut kube = local_candidate(ProviderKind::Kubernetes, node("ready"), 4, None);
        kube.room = Some(Room::Read(Fit {
            free_cpu: 763,
            free_memory: 660_602_880,
            needs_cpu: 1250,
            needs_memory: 1_610_612_736,
            fits: false,
            awaits_node: false,
        }));
        let decision = Placement::Place {
            provider: ProviderKind::DigitalOcean,
        };
        let e = event(&decision, &resolved, &[kube.clone(), droplet(0)]);
        assert_eq!(
            e["providers"][0]["capacity"],
            json!({
                "running": 4,
                "limit": null,
                "free_cpu": 763,
                "free_memory": 660_602_880u64,
                "needs_cpu": 1250,
                "needs_memory": 1_610_612_736u64,
                "fits": false,
            })
        );
        kube.room = Some(Room::Unread {
            unread: "the node may not read node general-1".into(),
        });
        let e = event(&decision, &resolved, &[kube]);
        assert_eq!(
            e["providers"][0]["capacity"]["unread"],
            "the node may not read node general-1"
        );
        assert!(e["providers"][0]["capacity"].get("fits").is_none());
    }

    #[test]
    fn a_card_is_raised_once_and_read_back() {
        let store = Store::open_in_memory().unwrap();
        let first = ask(
            &store,
            "n",
            "personal",
            "s1",
            "the gateway is down",
            ProviderKind::DigitalOcean,
            60,
        )
        .unwrap();
        let again = ask(
            &store,
            "n",
            "personal",
            "s1",
            "the gateway is down",
            ProviderKind::DigitalOcean,
            60,
        )
        .unwrap();
        assert_eq!(first.id, again.id);
        assert_eq!(answer(&store, &first.id).unwrap(), Answer::Waiting);
        let decided = store
            .decide_approval(
                &first.id,
                &crate::store::approvals::Decision {
                    state: crate::store::approvals::RUNNING,
                    option_id: crate::adapter::types::OPTION_ALLOW_ONCE,
                    bound_to: Some(&first.arguments),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(decided);
        assert_eq!(answer(&store, &first.id).unwrap(), Answer::Allowed);

        let other = ask(
            &store,
            "n",
            "personal",
            "s2",
            "x",
            ProviderKind::DigitalOcean,
            60,
        )
        .unwrap();
        store
            .decide_approval(
                &other.id,
                &crate::store::approvals::Decision {
                    state: crate::store::approvals::REJECTED,
                    option_id: crate::adapter::types::OPTION_REJECT_ONCE,
                    reason: Some("not today"),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            answer(&store, &other.id).unwrap(),
            Answer::Declined("not today".into())
        );
        assert!(matches!(
            answer(&store, "missing").unwrap(),
            Answer::Declined(_)
        ));
    }

    #[test]
    fn a_session_placed_here_holds_its_slot_until_its_start_returns() {
        let slots = Arc::new(Slots::default());
        let live: Vec<String> = Vec::new();
        let a = slots.starting("a");
        let b = slots.starting("b");
        // Not placed yet: holds nothing.
        assert_eq!(slots.running_locally(&live, "c"), 0);
        slots.local("a");
        slots.elsewhere("b");
        assert_eq!(slots.running_locally(&live, "c"), 1);
        assert_eq!(slots.running_locally(&live, "a"), 0);
        // Live now and still in `start`: counted once.
        let live = vec!["a".to_string(), "b".to_string()];
        assert_eq!(slots.running_locally(&live, "c"), 1);
        drop(a);
        drop(b);
        assert_eq!(slots.running_locally(&live, "c"), 1);
        // Ended: nothing left.
        assert_eq!(slots.running_locally(&[], "c"), 0);
    }

    #[test]
    fn a_start_that_fails_frees_its_slot() {
        let slots = Arc::new(Slots::default());
        let failed = slots.starting("a");
        slots.local("a");
        assert_eq!(slots.running_locally(&[], "b"), 1);
        drop(failed);
        assert_eq!(slots.running_locally(&[], "b"), 0);
    }

    #[test]
    fn a_session_starting_elsewhere_stays_elsewhere_once_live() {
        let slots = Arc::new(Slots::default());
        let _there = slots.starting("there");
        slots.elsewhere("there");
        // Another start counts while `there` is not live yet: it is not
        // forgotten as elsewhere.
        assert_eq!(slots.running_locally(&[], "other"), 0);
        let live = vec!["there".to_string()];
        assert_eq!(slots.running_locally(&live, "other"), 0);
    }

    /// A session placed here whose pod could not be scheduled, placed again
    /// on DigitalOcean, gives its slot here back.
    #[test]
    fn a_session_moved_elsewhere_gives_its_slot_back() {
        let slots = Arc::new(Slots::default());
        let _moved = slots.starting("moved");
        slots.local("moved");
        assert_eq!(slots.running_locally(&[], "other"), 1);
        slots.elsewhere("moved");
        assert_eq!(slots.running_locally(&[], "other"), 0);
        let live = vec!["moved".to_string()];
        assert_eq!(slots.running_locally(&live, "other"), 0);
    }
}
