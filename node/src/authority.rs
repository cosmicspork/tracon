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

pub fn policy_decision(
    policy: &Policy,
    channel: &str,
    action: &str,
    target: &str,
    args: &Value,
) -> Decision {
    policy.decide(&Request {
        channel,
        kind: Some("authority"),
        title: action,
        command: Some(target),
        arguments: Some(args),
    })
}

/// What is being decided: the caller, the scope, and the arguments the
/// summary is built from. Grouped so reordering eight positional arguments
/// cannot silently swap two of them.
pub struct AuthorityQuery<'a> {
    pub channel: &'a str,
    pub session_id: &'a str,
    pub action: &'a str,
    pub target: &'a str,
    pub revision: Option<&'a str>,
    pub args: &'a Value,
}

/// Decide at the last point before an external request.  Signed policy denial
/// and an active local denial both dominate every allow; missing grants ask.
pub fn decide(store: &Store, policy: &Policy, query: &AuthorityQuery) -> Result<Decision, String> {
    let policy_decision = policy_decision(
        policy,
        query.channel,
        query.action,
        query.target,
        query.args,
    );
    if policy.trusted && policy_decision.verdict == Verdict::Deny {
        return Ok(policy_decision);
    }
    let grants = store
        .authority_grants_for(
            query.channel,
            query.session_id,
            query.action,
            query.target,
            query.revision,
            now_ms(),
        )
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
    if !policy.trusted {
        return Ok(Decision {
            verdict: Verdict::Ask,
            rule_id: None,
            reason: Some("the signed policy bundle is unavailable or invalid".into()),
        });
    }
    if policy_decision.verdict == Verdict::Allow {
        return Ok(policy_decision);
    }
    if let Some(grant) = grants.iter().find(|g| g.verdict == "allow") {
        return Ok(Decision {
            verdict: Verdict::Allow,
            rule_id: Some(grant.id.clone()),
            reason: Some(grant.reason.clone()),
        });
    }
    Ok(Decision {
        verdict: Verdict::Ask,
        rule_id: None,
        reason: None,
    })
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

/// Whether a publish attempt failed because of the review's own state — stale
/// since submit, claimed by a concurrent approval, authority revoked
/// underneath it — or because the outside world it depended on returned an
/// error. Callers answer the two differently: a conflict invites the operator
/// to re-read the review, an external failure is the forge's fault, not
/// theirs.
#[derive(Debug)]
pub enum PublishError {
    Conflict(String),
    External(String),
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PublishError::Conflict(m) | PublishError::External(m) => f.write_str(m),
        }
    }
}

/// The node's own handles a publish needs, held apart from the
/// review-specific request so the two cannot be passed in the wrong order.
pub struct PublishContext<'a> {
    pub store: &'a Store,
    pub manager: &'a crate::session::Manager,
    pub broker: &'a crate::broker::SharedBroker,
    pub cfg: &'a crate::config::Config,
    pub node_id: &'a str,
}

/// What is being published, and how strictly. Automatic publication requires
/// node-recorded evidence and threads a recheck that re-reads authority right
/// before the atomic claim; explicit operator approval needs neither.
pub struct PublishRequest<'a> {
    pub review: &'a crate::store::ReviewRow,
    pub title: &'a str,
    pub body: &'a str,
    pub require_evidence: bool,
    pub recheck_authority: Option<&'a (dyn Fn() -> Result<(), String> + Send + Sync)>,
}

/// Publish the exact candidate captured by a review. This is shared by the
/// explicit operator approval and policy-authorized publication so neither can
/// skip freshness, the atomic claim, or the brokered SHA precondition.
pub async fn publish_review(
    ctx: &PublishContext<'_>,
    request: PublishRequest<'_>,
) -> Result<String, PublishError> {
    let PublishRequest {
        review,
        title,
        body,
        require_evidence,
        recheck_authority,
    } = request;
    if let Ok(Some(session)) = ctx.store.get_session(&review.session_id) {
        if session.node_id == ctx.node_id
            && session.harness_id != crate::session::external::HARNESS_ID
            && ctx
                .manager
                .snapshot_workspace(&review.session_id)
                .await
                .is_err()
        {
            return Err(PublishError::Conflict(
                "the runtime workspace could not be safely snapshotted".into(),
            ));
        }
    }
    let worktree = serde_json::from_str::<crate::review::publish::Target>(&review.target)
        .ok()
        .and_then(|target| target.worktree)
        .or_else(|| {
            ctx.store
                .get_session(&review.session_id)
                .ok()
                .flatten()
                .and_then(|session| session.worktree_path)
        })
        .ok_or_else(|| PublishError::Conflict("the worktree is gone".into()))?;
    let files: Vec<crate::review::FileAtSubmit> =
        serde_json::from_str(&review.files).unwrap_or_default();
    let stale = crate::review::staleness(&worktree, &review.head_sha, &files).await;
    if !stale.is_empty() {
        return Err(PublishError::Conflict(format!(
            "changed since submit: {}",
            stale.join(", ")
        )));
    }
    if require_evidence {
        crate::review::checks::review_required_checks_current(
            ctx.store,
            ctx.manager.backend().as_ref(),
            &review.id,
            ctx.cfg,
        )
        .await
        .map_err(PublishError::Conflict)?;
    }
    if let Some(recheck) = recheck_authority {
        recheck().map_err(PublishError::Conflict)?;
    }
    // Bind the revision this call validated immediately before claiming the
    // publish, so a resubmit racing in between loses the atomic claim rather
    // than having its bytes silently attributed to this approval.
    let revision_id = ctx
        .store
        .latest_review_revision(&review.id)
        .map_err(|e| PublishError::External(e.to_string()))?
        .map(|revision| revision.id);
    if !ctx
        .store
        .begin_publish(&review.id, revision_id.as_deref())
        .map_err(|e| PublishError::External(e.to_string()))?
    {
        return Err(PublishError::Conflict(
            "this review is already being decided".into(),
        ));
    }
    let target: crate::review::publish::Target =
        serde_json::from_str(&review.target).map_err(|e| PublishError::External(e.to_string()))?;
    match crate::review::publish::publish(
        ctx.broker,
        ctx.cfg,
        &review.channel,
        ctx.node_id,
        &review.id,
        &worktree,
        &target,
        &review.head_sha,
        title,
        body,
        recheck_authority,
    )
    .await
    {
        Ok(published) => {
            if !ctx
                .store
                .finish_publish(&review.id, title, body, &published)
                .map_err(|e| PublishError::External(e.to_string()))?
            {
                return Err(PublishError::Conflict(
                    "publish completed externally, but the review claim was lost; reconcile it before retrying".into(),
                ));
            }
            ctx.manager.publish_queue().await;
            if let Some(item) = ctx
                .store
                .get_session(&review.session_id)
                .map_err(|e| PublishError::External(e.to_string()))?
                .and_then(|session| session.work_item_id)
            {
                if crate::corpus::work::close(
                    ctx.store,
                    ctx.manager.bus(),
                    ctx.node_id,
                    &item,
                    Some(&review.session_id),
                )
                .is_ok()
                {
                    ctx.manager
                        .item_closed(&review.session_id, &format!("published: {published}"))
                        .await;
                }
            }
            Ok(published)
        }
        Err(error) => {
            ctx.store
                .abort_publish(&review.id)
                .map_err(|e| PublishError::External(e.to_string()))?;
            ctx.manager.publish_queue().await;
            // A branch that moved between the reviewed tip and this push is a
            // conflict with what was approved, not a forge failure.
            Err(match error {
                crate::review::publish::PublishError::BranchMoved { .. } => {
                    PublishError::Conflict(error.to_string())
                }
                other => PublishError::External(other.to_string()),
            })
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
        store
            .authority_grant_insert(&grant("allow", "allow"))
            .unwrap();
        store.authority_grant_insert(&grant("ask", "ask")).unwrap();
        let decision = decide(
            &store,
            &Policy::shipped(),
            &AuthorityQuery {
                channel: "personal",
                session_id: "session",
                action: MERGE,
                target: "github:me/project:pr:7",
                revision: Some("abc1234"),
                args: &serde_json::json!({}),
            },
        )
        .unwrap();
        assert_eq!(decision.verdict, Verdict::Ask);
        assert_eq!(decision.rule_id.as_deref(), Some("ask"));
    }

    #[test]
    fn unsigned_policy_cannot_activate_local_grant() {
        let store = Store::open_in_memory().unwrap();
        store
            .authority_grant_insert(&grant("allow", "allow"))
            .unwrap();
        let decision = decide(
            &store,
            &Policy::default(),
            &AuthorityQuery {
                channel: "personal",
                session_id: "session",
                action: MERGE,
                target: "github:me/project:pr:7",
                revision: Some("abc1234"),
                args: &serde_json::json!({}),
            },
        )
        .unwrap();
        assert_eq!(decision.verdict, Verdict::Ask);
    }
}
