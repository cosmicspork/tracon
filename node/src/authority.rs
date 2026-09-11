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
    if !policy.trusted {
        return Ok(Decision {
            verdict: Verdict::Ask,
            rule_id: None,
            reason: Some("the signed policy bundle is unavailable or invalid".into()),
        });
    }
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
    if let Some(grant) = grants.iter().find(|g| g.verdict == "ask") {
        return Ok(Decision {
            verdict: Verdict::Ask,
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

/// Publish the exact candidate captured by a review. This is shared by the
/// explicit operator approval and policy-authorized publication so neither can
/// skip freshness, the atomic claim, or the brokered SHA precondition.
pub async fn publish_review(
    store: &Store,
    manager: &crate::session::Manager,
    broker: &crate::broker::SharedBroker,
    cfg: &crate::config::Config,
    node_id: &str,
    review: &crate::store::ReviewRow,
    title: &str,
    body: &str,
    require_evidence: bool,
    recheck_authority: Option<&dyn Fn() -> Result<(), String>>,
) -> Result<String, String> {
    let worktree = serde_json::from_str::<crate::review::publish::Target>(&review.target)
        .ok()
        .and_then(|target| target.worktree)
        .or_else(|| {
            store.get_session(&review.session_id).ok().flatten()
                .and_then(|session| session.worktree_path)
        })
        .ok_or("the worktree is gone")?;
    let files: Vec<crate::review::FileAtSubmit> =
        serde_json::from_str(&review.files).unwrap_or_default();
    let stale = crate::review::staleness(&worktree, &review.head_sha, &files).await;
    if !stale.is_empty() {
        return Err(format!("changed since submit: {}", stale.join(", ")));
    }
    if require_evidence {
        crate::review::checks::review_required_checks_current(store, &review.id, cfg)?;
    }
    if let Some(recheck) = recheck_authority {
        recheck()?;
    }
    if !store.begin_publish(&review.id).map_err(|e| e.to_string())? {
        return Err("this review is already being decided".into());
    }
    let target: crate::review::publish::Target =
        serde_json::from_str(&review.target).map_err(|e| e.to_string())?;
    match crate::review::publish::publish(
        broker,
        cfg,
        &review.channel,
        node_id,
        &worktree,
        &target,
        &review.head_sha,
        title,
        body,
    )
    .await {
        Ok(published) => {
            store.finish_publish(&review.id, title, body, &published)
                .map_err(|e| e.to_string())?;
            manager.publish_queue().await;
            if let Some(item) = store.get_session(&review.session_id).map_err(|e| e.to_string())?
                .and_then(|session| session.work_item_id)
            {
                if crate::corpus::work::close(
                    store,
                    manager.bus(),
                    node_id,
                    &item,
                    Some(&review.session_id),
                ).is_ok() {
                    manager.item_closed(&review.session_id, &format!("published: {published}")).await;
                }
            }
            Ok(published)
        }
        Err(error) => {
            store.abort_publish(&review.id).map_err(|e| e.to_string())?;
            manager.publish_queue().await;
            Err(error.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(id: &str, verdict: &str) -> AuthorityGrantRow {
        AuthorityGrantRow {
            id: id.into(),
            action: MERGE.into(),
            verdict: verdict.into(),
            target: "github:me/project:pr:7".into(),
            channel: "personal".into(),
            session_id: None,
            revision: Some("abc1234".into()),
            expires_ms: None,
            revoked_ms: None,
            reason: id.into(),
            created_ms: now_ms(),
        }
    }

    #[test]
    fn scoped_ask_overrides_broader_allow() {
        let store = Store::open_in_memory().unwrap();
        store.authority_grant_insert(&grant("allow", "allow")).unwrap();
        store.authority_grant_insert(&grant("ask", "ask")).unwrap();
        let decision = decide(
            &store, &Policy::shipped(), "personal", "session", MERGE,
            "github:me/project:pr:7", Some("abc1234"), &serde_json::json!({}),
        ).unwrap();
        assert_eq!(decision.verdict, Verdict::Ask);
        assert_eq!(decision.rule_id.as_deref(), Some("ask"));
    }

    #[test]
    fn unsigned_policy_cannot_activate_local_grant() {
        let store = Store::open_in_memory().unwrap();
        store.authority_grant_insert(&grant("allow", "allow")).unwrap();
        let decision = decide(
            &store, &Policy::default(), "personal", "session", MERGE,
            "github:me/project:pr:7", Some("abc1234"), &serde_json::json!({}),
        ).unwrap();
        assert_eq!(decision.verdict, Verdict::Ask);
    }
}
