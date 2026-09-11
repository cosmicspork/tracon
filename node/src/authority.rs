//! Narrow, durable authority for consequential brokered operations.
//!
//! A grant is a local operator decision, not a policy edit: signed policy still
//! decides first and an invalid bundle still fails closed.  Every use reads the
//! grant at dispatch time, so expiry and revocation take effect immediately.

use serde_json::Value;

use crate::{
    policy::{Decision, Policy, Request, Verdict},
    store::{now_ms, AuthorityGrantRow, Store},
};

pub const MERGE: &str = "merge";
pub const PUBLISH: &str = "publish";
pub const TICKET_TRANSITION: &str = "ticket_transition";
pub const DEPLOY: &str = "deploy";
pub const BROWSER_VERIFY: &str = "browser_verify";
pub const BROWSER_TEST_ACCOUNT: &str = "browser_test_account";

pub fn valid_action(action: &str) -> bool {
    matches!(
        action,
        MERGE | PUBLISH | TICKET_TRANSITION | DEPLOY | BROWSER_VERIFY | BROWSER_TEST_ACCOUNT
    )
}

pub fn target(kind: &str, parts: &[&str]) -> String {
    format!("{kind}:{}", parts.join(":"))
}

pub fn policy_decision(policy: &Policy, channel: &str, action: &str, target: &str, args: &Value) -> Decision {
    policy.decide(&Request {
        channel,
        kind: Some("authority"),
        title: action,
        command: Some(target),
        arguments: Some(args),
    })
}

/// Decide at the last point before an external request.  Signed policy denial
/// and an active local denial both dominate every allow; missing grants ask.
pub fn decide(
    store: &Store,
    policy: &Policy,
    channel: &str,
    session_id: &str,
    action: &str,
    target: &str,
    revision: Option<&str>,
    args: &Value,
) -> Result<Decision, String> {
    let policy = policy_decision(policy, channel, action, target, args);
    if policy.verdict == Verdict::Deny {
        return Ok(policy);
    }
    let grants = store
        .authority_grants_for(channel, session_id, action, target, revision, now_ms())
        .map_err(|e| e.to_string())?;
    if let Some(grant) = grants.iter().find(|g| g.verdict == "deny") {
        return Ok(Decision {
            verdict: Verdict::Deny,
            rule_id: Some(grant.id.clone()),
            reason: Some(grant.reason.clone()),
        });
    }
    if policy.verdict == Verdict::Allow {
        return Ok(policy);
    }
    if let Some(grant) = grants.iter().find(|g| g.verdict == "allow") {
        return Ok(Decision {
            verdict: Verdict::Allow,
            rule_id: Some(grant.id.clone()),
            reason: Some(grant.reason.clone()),
        });
    }
    Ok(Decision { verdict: Verdict::Ask, rule_id: None, reason: None })
}

pub fn grant_visible(row: &AuthorityGrantRow) -> Value {
    serde_json::json!({
        "id": row.id,
        "action": row.action,
        "verdict": row.verdict,
        "target": row.target,
        "channel": row.channel,
        "session_id": row.session_id,
        "revision": row.revision,
        "expires_ms": row.expires_ms,
        "revoked_ms": row.revoked_ms,
        "reason": row.reason,
        "created_ms": row.created_ms,
    })
}
