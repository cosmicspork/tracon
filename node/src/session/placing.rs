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

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::provider::digitalocean::runtime::DoRuntime;
use crate::provider::requirements::Resolved;
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

/// Whether placement decides anything for this session.
pub fn in_play(digitalocean: bool, resolved: &Resolved) -> bool {
    digitalocean || resolved.requirements != Requirements::any()
}

/// The node's own runtime as placement sees it: what a verified container
/// boundary gives, healthy while the node's checks pass, full at
/// `[placement] local_limit`.
pub fn local_candidate(
    kind: ProviderKind,
    health: Result<(), String>,
    running: u32,
    limit: Option<u32>,
) -> Candidate {
    use crate::provider::capabilities::{
        Capabilities, Dns, ImageIdentityKind, Privilege, Route, RuntimeSocket, Tier,
    };
    Candidate {
        kind,
        capabilities: Capabilities {
            tier: Tier::Container,
            route: Route::NodeOnly,
            dns: Dns::None,
            privilege: Privilege::Unprivileged,
            runtime_socket: RuntimeSocket::Absent,
            image_identity: ImageIdentityKind::Exact,
        },
        capacity: Capacity { running, limit },
        health,
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
            json!({
                "provider": c.kind,
                "capacity": c.capacity,
                "healthy": c.health.is_ok(),
                "why": c.health.as_ref().err(),
            })
        })
        .collect();
    payload
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::digitalocean;
    use crate::provider::requirements::DoMode;

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
}
