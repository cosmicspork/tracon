//! What one session may do, in the terms the operator thinks in.
//!
//! The question behind most low-value interruptions is not "may this call go
//! ahead" — it is "what is this session allowed to do at all, and why is *this*
//! the thing it has to ask me about". Answering that meant reading the config,
//! the bundle and the grants table separately and holding the join in your
//! head, so the interface never answered it and the operator approved without
//! it.
//!
//! Everything here is derived at read time from the same policy, grants,
//! broker and configuration the gate consults. Nothing is a second model of
//! what the node believes it does: every verdict below comes from calling
//! [`crate::policy::Policy::explain`] or [`crate::authority::decide`], the
//! functions that decide the real call. A description maintained beside the
//! enforcement is worse than none, because it is believed.
//!
//! It grants nothing. This is a read of state that already exists — no route
//! here widens what a session may do unattended, least of all an external
//! write.

use serde::Serialize;
use serde_json::{json, Value};

use crate::{
    authority,
    config::Config,
    mcp::Tools,
    policy::{Request, Standing, UnattendedCommands},
    store::{now_ms, NodeRow, SessionRow, Store},
};

/// One thing the session can name, and what the node would answer now.
#[derive(Debug, Clone, Serialize)]
pub struct ActionStanding {
    pub name: String,
    /// `tool` for a brokered call, `authority` for a consequential external
    /// action, `capability` for a surface like a terminal.
    pub surface: String,
    #[serde(flatten)]
    pub standing: Standing,
    /// Authority actions only: the live grants that bear on this action for
    /// this session, so "asked" and "asked unless a grant covers the target"
    /// are distinguishable.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub grants: Vec<Value>,
}

/// What a session may do. Assembled per request; nothing is cached, because a
/// grant revoked a second ago must not be explained as live.
#[derive(Debug, Clone, Serialize)]
pub struct SessionAuthority {
    pub session_id: String,
    pub channel: String,
    pub node: Value,
    pub harness: Value,
    pub image: Value,
    pub access: Value,
    pub limits: Value,
    pub policy: Value,
    /// Every named action, with what the node would answer for it now.
    pub actions: Vec<ActionStanding>,
    /// Commands that run unattended, as the bundle's own allow rules name them.
    pub unattended_commands: Vec<UnattendedCommands>,
    /// Live grants for this session or its channel, whatever they name.
    pub grants: Vec<Value>,
}

/// The consequential actions and capabilities a grant can carry.
///
/// This is a second list, which is the defect this whole module exists to
/// avoid, so it is not left to care: `authority_explained` asserts that every
/// action [`crate::authority::valid_action`] accepts appears here. An action
/// added to the node and missed here fails that test rather than quietly
/// going unexplained.
const AUTHORITY_ACTIONS: &[&str] = &[
    authority::MERGE,
    authority::PUBLISH,
    authority::TICKET_TRANSITION,
    authority::DEPLOY,
    authority::BROWSER_VERIFY,
    authority::BROWSER_TEST_ACCOUNT,
    authority::TERMINAL,
];

pub fn session_authority(
    store: &Store,
    tools: &Tools,
    cfg: &Config,
    session: &SessionRow,
    node: Option<&NodeRow>,
) -> SessionAuthority {
    let policy = tools.policy.read();
    let phase = match session.phase.as_str() {
        "plan" => crate::session::Phase::Plan,
        "review" => crate::session::Phase::Review,
        _ => crate::session::Phase::Execute,
    };
    // The exact definitions this session is offered, from the same call the
    // harness's `tools/list` is answered with: a tool the channel has no
    // credential for is not offered, and so is not explained as available.
    let offered = tools.list_for(&session.channel, &session.node_id, phase);
    let mut actions: Vec<ActionStanding> = offered
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .map(|name| ActionStanding {
            name: name.to_string(),
            surface: crate::mcp::TOOL_KIND.to_string(),
            // No arguments, not empty ones: this is the standing of the
            // action itself, and a placeholder value would join the haystack a
            // deny rule matches on. An argument-scoped allow shows up as
            // `scoped` rather than being reported as an allow the call may not
            // actually get.
            standing: policy.explain(&Request {
                channel: &session.channel,
                action: name,
                kind: Some(crate::mcp::TOOL_KIND),
                resource: None,
                command: None,
                arguments: None,
            }),
            grants: Vec::new(),
        })
        .collect();

    let live: Vec<_> = store
        .authority_grants(false)
        .unwrap_or_default()
        .into_iter()
        .filter(|g| g.channel == session.channel)
        .filter(|g| g.session_id.is_none() || g.session_id.as_deref() == Some(&session.id))
        .filter(|g| g.expires_ms.is_none_or(|at| at > now_ms()))
        .collect();

    for action in AUTHORITY_ACTIONS {
        let kind = authority::kind_of(action);
        // The bundle's answer for the action itself. A grant can only ever
        // narrow the gap between this and an allow, never override a denial,
        // which is why the two are reported side by side rather than merged
        // into one verdict the operator cannot take apart.
        let standing = policy.explain(&Request {
            channel: &session.channel,
            action,
            kind: Some(kind),
            resource: None,
            command: None,
            arguments: None,
        });
        let grants: Vec<Value> = live
            .iter()
            .filter(|g| g.action == *action)
            .map(authority::grant_visible)
            .collect();
        actions.push(ActionStanding {
            name: (*action).to_string(),
            surface: kind.to_string(),
            standing,
            grants,
        });
    }

    let broker = tools.broker.read().unwrap();
    let credentials = broker.available_to(&session.channel, &session.node_id);
    let bindings = json!({
        "credentials": credentials,
        // Hosts the gateway will let the harness CONNECT to. Everything else
        // is refused at the boundary, before policy is consulted at all.
        "egress": cfg.gateway.allow_hosts,
        "workspace": session.worktree_path.clone().unwrap_or_else(|| session.repo_path.clone()),
        "repo": session.repo_path,
        "branch": session.branch,
        "external_broker": cfg.external.enabled,
    });

    SessionAuthority {
        session_id: session.id.clone(),
        channel: session.channel.clone(),
        node: json!({
            "id": session.node_id,
            "name": node.map(|n| n.name.clone()),
            "is_self": node.is_some_and(|n| n.is_self == 1),
            "reachable": node.is_none_or(|n| n.reachable == 1),
            "isolation": node.map(|n| n.state.clone()),
            "failed_check": node.and_then(|n| n.failed_check.clone()),
            "failed_detail": node.and_then(|n| n.failed_detail.clone()),
        }),
        harness: json!({
            "id": session.harness_id,
            "expected": session.harness_version,
            "found": session.harness_found,
            "agent": session.harness_agent,
            "protocol": session.harness_protocol,
        }),
        image: json!({
            "runtime": cfg.runtime.kind,
            "execution": execution_image(cfg),
            "manifest_digest": session.manifest_digest,
        }),
        access: bindings,
        limits: json!({
            "budget_tokens": session.budget_tokens,
            "tokens_used": session.tokens_used,
            "permission_timeout_secs": cfg.session.permission_timeout_secs,
            "ceiling": crate::metrics::ceiling(
                store,
                &channel_bindings(store, &session.channel),
                &session.channel,
            ),
        }),
        policy: json!({
            "version": policy.version,
            // Untrusted policy yields no rules, so everything is asked. Saying
            // so is the difference between "this node asks a lot" and "this
            // node's bundle is broken".
            "trusted": policy.trusted,
            "rules": policy.rules.len(),
        }),
        unattended_commands: policy.unattended_commands(&session.channel),
        actions,
        grants: live.iter().map(authority::grant_visible).collect(),
    }
}

/// A channel's bindings, read the way `Manager::bindings` reads them. Taken
/// from the store rather than through the manager because this path has no
/// manager and does not need one: the row is the same row.
fn channel_bindings(store: &Store, channel: &str) -> Value {
    store
        .channel_get(channel)
        .ok()
        .flatten()
        .and_then(|c| serde_json::from_str(&c.bindings_json).ok())
        .unwrap_or_else(|| json!({}))
}

/// The image the boundary would launch a harness from under the configured
/// runtime. Two fields hold it, and reporting the wrong one would describe an
/// isolation the session is not in.
fn execution_image(cfg: &Config) -> String {
    match cfg.runtime.kind {
        crate::config::RuntimeKind::Kubernetes => cfg.runtime.kubernetes.harness_image.clone(),
        _ => cfg.boundary.harness_image.clone(),
    }
}
