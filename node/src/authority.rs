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
/// An interactive terminal inside one session's workspace. Unlike the forge
/// actions, this one authorises a *surface* rather than a single external
/// side effect: `POST /pty` is arbitrary command execution with no permission
/// check of its own (finding 7), so the grant is the only thing between the
/// native UI and a shell. It is therefore bound to a session and to that
/// session's workspace path, and it expires.
pub const TERMINAL: &str = "terminal";

/// The policy kind the forge actions are decided under.
pub const AUTHORITY_KIND: &str = "authority";
/// The policy kind a capability is decided under. A terminal is not a forge
/// operation, and a bundle rule that names it should read as what it is.
pub const CAPABILITY_KIND: &str = "capability";

pub fn valid_action(action: &str) -> bool {
    matches!(
        action,
        MERGE
            | PUBLISH
            | TICKET_TRANSITION
            | DEPLOY
            | BROWSER_VERIFY
            | BROWSER_TEST_ACCOUNT
            | TERMINAL
    )
}

/// The policy kind one action is decided under. Keeping the mapping here is
/// what stops a capability grant and a capability rule disagreeing about which
/// name the bundle should be consulted under.
pub fn kind_of(action: &str) -> &'static str {
    match action {
        TERMINAL => CAPABILITY_KIND,
        _ => AUTHORITY_KIND,
    }
}

pub fn target(kind: &str, parts: &[&str]) -> String {
    format!("{kind}:{}", parts.join(":"))
}

/// The target a terminal grant binds to: this session and the exact workspace
/// path the gateway pins every request to. A session whose workspace moved no
/// longer matches the grant it was given, which is the point — the grant is of
/// a terminal *in that directory*, not of a terminal in general.
pub fn terminal_target(session_id: &str, workspace: &str) -> String {
    target(TERMINAL, &[session_id, workspace])
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
        action,
        kind: Some(kind_of(action)),
        resource: Some(target),
        command: None,
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
    /// Neither: the node could not establish what happened on the forge. The
    /// publish claim is deliberately left standing and the publication record
    /// says what to verify, because reporting either outcome would be a guess.
    Uncertain(String),
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PublishError::Conflict(m) | PublishError::External(m) | PublishError::Uncertain(m) => {
                f.write_str(m)
            }
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
    /// The revision this approval was given for, as the deciding caller read
    /// it. An approval is of a particular set of bytes, not of a review id:
    /// when the agent resubmits between the operator reading the diff and
    /// this call, the approval belongs to what they read, so it is refused
    /// rather than transferred to bytes nobody looked at. `None` means the
    /// caller has no earlier reading to bind to and takes the latest.
    pub decided_revision_id: Option<&'a str>,
}

/// The publication record, as `review::publish` needs to speak to it: each
/// external side effect is written down before it is attempted. Keeping it
/// here is what lets publication stay free of the database.
struct StoreJournal<'a> {
    store: &'a Store,
    id: &'a str,
}

impl crate::review::publish::Journal for StoreJournal<'_> {
    fn pushed(&self, sha: &str) -> Result<(), String> {
        self.store
            .publication_pushed(self.id, sha)
            .map_err(|e| e.to_string())
    }

    fn opening(&self) -> Result<(), String> {
        self.store
            .publication_opening(self.id)
            .map_err(|e| e.to_string())
    }

    fn opened(&self, url: &str) -> Result<(), String> {
        self.store
            .publication_opened(self.id, url)
            .map_err(|e| e.to_string())
    }

    fn uncertain(&self, note: &str) {
        if let Err(error) = self.store.publication_uncertain(self.id, note) {
            tracing::error!(%error, "could not record an uncertain publication");
        }
    }

    fn failed(&self, note: &str) {
        if let Err(error) = self.store.publication_failed(self.id, note) {
            tracing::error!(%error, "could not record a failed publication");
        }
    }
}

/// One publication's identity: the same review, revision, target and commit
/// name the same publication, so a retry after a crash looks for what the
/// interrupted attempt would have left on the forge instead of starting a
/// second one. A resubmission has a new revision, so it is a new publication.
pub fn publication_id(
    review: &crate::store::ReviewRow,
    revision_id: Option<&str>,
    target: &crate::review::publish::Target,
) -> String {
    crate::corpus::hash_body(&format!(
        "{}\n{}\n{}\n{}\n{}\n{}\n{}",
        review.id,
        revision_id.unwrap_or(""),
        target.provider,
        target.project,
        target.base,
        target.branch,
        review.head_sha,
    ))
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
        decided_revision_id,
    } = request;
    // Before anything is read from the worktree or the forge: if a newer
    // revision landed after the decision was made, this approval is for bytes
    // that are no longer what the review holds. Refuse it here, where the
    // message can say so, rather than letting the claim fail opaquely later.
    if let Some(decided) = decided_revision_id {
        let current = ctx
            .store
            .latest_review_revision(&review.id)
            .map_err(|e| PublishError::External(e.to_string()))?
            .map(|revision| revision.id);
        if current.as_deref() != Some(decided) {
            return Err(PublishError::Conflict(
                "a newer revision was submitted after this approval was decided; \
                 review the new one before publishing"
                    .into(),
            ));
        }
    }
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
    // than having its bytes silently attributed to this approval. When the
    // caller decided against a particular revision, that one is what is
    // claimed — never one that arrived since.
    let revision_id = match decided_revision_id {
        Some(decided) => Some(decided.to_string()),
        None => ctx
            .store
            .latest_review_revision(&review.id)
            .map_err(|e| PublishError::External(e.to_string()))?
            .map(|revision| revision.id),
    };
    let target: crate::review::publish::Target =
        serde_json::from_str(&review.target).map_err(|e| PublishError::External(e.to_string()))?;
    // The tree the node hashed out of the candidate when it captured this
    // review. Publication compares the bytes it is about to push against it,
    // so the reviewed tree is asserted from node-held evidence rather than
    // re-read out of the directory being published. A candidate with none
    // recorded — a review captured before candidates were, and never
    // resubmitted since — is refused here rather than published on the word
    // of the directory being published.
    let candidate_id = crate::store::candidate_id(&review.head_sha, &review.channel);
    let reviewed_tree = ctx
        .store
        .candidate(&candidate_id)
        .map_err(|e| PublishError::External(e.to_string()))?
        .and_then(|candidate| candidate.tree_sha)
        .filter(|tree| !tree.is_empty())
        .ok_or_else(|| {
            PublishError::Conflict(
                "no reviewed tree is recorded for this candidate, so what would be pushed cannot \
                 be checked against what was reviewed; resubmit it for review before publishing"
                    .into(),
            )
        })?;

    let publication_id = publication_id(review, revision_id.as_deref(), &target);
    let existing = ctx
        .store
        .publication(&publication_id)
        .map_err(|e| PublishError::External(e.to_string()))?;
    // An attempt another process left in flight is not a competing decision:
    // it is this node's own, interrupted. Take the claim back and resume it.
    let interrupted = existing
        .as_ref()
        .is_some_and(|row| row.instance != crate::process::instance_id());
    let claimed = ctx
        .store
        .begin_publish(&review.id, revision_id.as_deref())
        .map_err(|e| PublishError::External(e.to_string()))?
        || (interrupted
            && ctx
                .store
                .reclaim_publish(&review.id, revision_id.as_deref())
                .map_err(|e| PublishError::External(e.to_string()))?);
    if !claimed {
        return Err(PublishError::Conflict(
            "this review is already being decided".into(),
        ));
    }
    // Already done. A second attempt at an opened publication would open a
    // second change; report the one that exists instead.
    if let Some(url) = existing
        .as_ref()
        .filter(|row| row.state == "opened")
        .and_then(|row| row.result.clone())
    {
        ctx.store
            .finish_publish(&review.id, title, body, &url)
            .map_err(|e| PublishError::External(e.to_string()))?;
        ctx.manager.publish_queue().await;
        return Ok(url);
    }
    // What a previous attempt may have left on the forge decides whether this
    // one looks before it acts.
    let resume = existing
        .as_ref()
        .is_some_and(|row| row.may_have_reached_the_forge());
    let record = ctx
        .store
        .publication_begin(&crate::store::PublicationBegin {
            id: &publication_id,
            review_id: &review.id,
            revision_id: revision_id.as_deref(),
            candidate_id: &candidate_id,
            channel: &review.channel,
            node_id: ctx.node_id,
            provider: &target.provider,
            project: &target.project,
            base: &target.base,
            branch: &target.branch,
            head_sha: &review.head_sha,
            instance: crate::process::instance_id(),
        })
        .map_err(|e| PublishError::External(e.to_string()))?;
    let journal = StoreJournal {
        store: ctx.store,
        id: &publication_id,
    };
    match crate::review::publish::publish(
        ctx.broker,
        ctx.cfg,
        &crate::review::publish::Publication {
            channel: &review.channel,
            node_id: ctx.node_id,
            id: &publication_id,
            candidate: &worktree,
            target: &target,
            head_sha: &review.head_sha,
            reviewed_tree: &reviewed_tree,
            title,
            body,
            resume,
            pushed: record.pushed_sha.is_some(),
            before_push: recheck_authority,
            journal: &journal,
        },
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
        // The outcome is unknown, so the claim stays: releasing it would
        // invite a second attempt at a side effect that may already have
        // landed. The publication record holds what to verify.
        Err(error) if error.is_unknown() => {
            ctx.manager.publish_queue().await;
            Err(PublishError::Uncertain(error.to_string()))
        }
        Err(error) => {
            ctx.store
                .abort_publish(&review.id)
                .map_err(|e| PublishError::External(e.to_string()))?;
            ctx.manager.publish_queue().await;
            // A branch that moved between the reviewed tip and this push is a
            // conflict with what was approved, not a forge failure.
            Err(match error {
                crate::review::publish::PublishError::BranchMoved { .. }
                | crate::review::publish::PublishError::TreeChanged { .. }
                | crate::review::publish::PublishError::NoReviewedTree => {
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
