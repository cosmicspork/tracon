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
        MERGE | PUBLISH | TICKET_TRANSITION | DEPLOY | TERMINAL
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
    pub session_id: Option<&'a str>,
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
    /// The operator's edits to what goes to the forge. `None` publishes the
    /// revision's own outputs.
    pub outputs: Option<&'a crate::review::publish::Outputs>,
    /// The operator's explicit recovery of this exact publication, as its
    /// journal recorded it. It may take back a claim an uncertain attempt of
    /// this process left standing, which nothing else may: that claim is what
    /// stops a second approval from repeating a side effect blind, and a
    /// recovery is the one caller that looks at the forge before it acts.
    pub recover: bool,
}

/// Where an approval landed, and exactly what it sent to the forge.
#[derive(Debug, Clone)]
pub struct Published {
    pub url: String,
    pub outputs: crate::review::publish::Outputs,
}

/// A revision's publication intent. A revision from before intents existed
/// has none, and asks only for a new change described by its title and body.
pub fn revision_intent(
    store: &Store,
    revision_id: Option<&str>,
) -> Result<crate::review::publish::Intent, String> {
    let Some(id) = revision_id else {
        return Ok(Default::default());
    };
    match store
        .review_revision(id)
        .map_err(|e| e.to_string())?
        .and_then(|revision| revision.intent_json)
    {
        Some(json) => serde_json::from_str(&json)
            .map_err(|e| format!("this revision's publication intent is unreadable: {e}")),
        None => Ok(Default::default()),
    }
}

/// The publication record, as `review::publish` needs to speak to it: each
/// external side effect is written down before it is attempted. Keeping it
/// here is what lets publication stay free of the database.
struct StoreJournal<'a> {
    store: &'a Store,
    id: &'a str,
}

impl crate::review::publish::Journal for StoreJournal<'_> {
    fn contacting(&self) -> Result<(), String> {
        self.store
            .publication_contacting(self.id)
            .map_err(|e| e.to_string())
    }

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

/// Whether this node could publish a review at all, as far as the node can
/// tell without asking the forge: the provider is one it publishes to, and a
/// credential for it is bound to the review's channel on this node. A bound
/// token is not permission to push; only the forge knows that, and the
/// wording here never claims more than the binding.
pub fn publication_readiness(
    broker: &crate::broker::SharedBroker,
    review: &crate::store::ReviewRow,
    node_id: &str,
) -> serde_json::Value {
    let target = serde_json::from_str::<crate::review::publish::Target>(&review.target).ok();
    let settings = format!("/settings?node={node_id}#connections");
    let Some(target) = target else {
        return serde_json::json!({
            "ready": false,
            "problem": "this review names no publication target",
            "settings": settings,
        });
    };
    let Some(provider) = crate::review::publish::Provider::parse(&target.provider) else {
        return serde_json::json!({
            "ready": false,
            "provider": target.provider,
            "project": target.project,
            "problem": format!("{} is not a forge this node publishes to", target.provider),
            "settings": settings,
        });
    };
    let credential = provider.credential();
    let forge = forge_name(provider);
    let bound = broker
        .read()
        .unwrap()
        .env_for(credential, &review.channel, node_id)
        .map(|_| ());
    let problem = match bound {
        Ok(()) => None,
        Err(crate::broker::BrokerError::Unknown(_)) => Some(format!(
            "no {forge} token ({credential}) is stored on this node"
        )),
        Err(crate::broker::BrokerError::NotBound { .. }) => Some(format!(
            "the {forge} token ({credential}) is not bound to the {} channel",
            review.channel
        )),
        Err(crate::broker::BrokerError::NotOnThisNode { .. }) => Some(format!(
            "the {forge} token ({credential}) is not bound to this node"
        )),
        Err(other) => Some(other.to_string()),
    };
    serde_json::json!({
        "ready": problem.is_none(),
        "provider": provider.as_str(),
        "credential": credential,
        "project": target.project,
        "problem": problem,
        "note": format!(
            "A {forge} token is bound to this channel. Whether it may push to {} is only \
             known when {forge} answers.",
            target.project
        ),
        "settings": settings,
    })
}

fn forge_name(provider: crate::review::publish::Provider) -> &'static str {
    match provider {
        crate::review::publish::Provider::Github => "GitHub",
        crate::review::publish::Provider::Gitlab => "GitLab",
    }
}

/// Why the operator may not retry this publication as it was approved, or
/// `None` when they may. A retry sends the recorded title, body and outputs
/// of the recorded revision, so anything that changed since — a newer
/// revision, a moved branch, another target, prose from before it was
/// recorded — needs a fresh approval instead.
pub fn recovery_refusal(
    store: &Store,
    review: &crate::store::ReviewRow,
    row: &crate::store::PublicationRow,
) -> Option<String> {
    use crate::store::PublicationOutcome as O;
    match row.outcome(crate::process::instance_id()) {
        O::Published => return Some("this publication already landed".into()),
        O::InProgress => return Some("an attempt at this publication is running now".into()),
        O::NotAttempted | O::Failed | O::Uncertain => {}
    }
    if !matches!(review.state.as_str(), "new" | "claimed" | "publishing") {
        return Some(format!(
            "the review was settled since ({}); there is nothing left to retry",
            review.state
        ));
    }
    let latest = store
        .latest_review_revision(&review.id)
        .ok()
        .flatten()
        .map(|revision| revision.id);
    if latest != row.revision_id || review.head_sha != row.head_sha {
        return Some(
            "a newer revision was submitted since this was approved; approving it is a new \
             decision"
                .into(),
        );
    }
    let same_target = serde_json::from_str::<crate::review::publish::Target>(&review.target)
        .ok()
        .is_some_and(|target| {
            publication_id(review, row.revision_id.as_deref(), &target) == row.id
        });
    if !same_target {
        return Some("the review's publication target changed; approve it again".into());
    }
    if row.title.is_none() || row.body.is_none() || row.outputs_json.is_none() {
        return Some(
            "this attempt predates the record of what was approved, so a retry cannot know what \
             to send; approve it again"
                .into(),
        );
    }
    None
}

/// A review's latest publication as the operator should see it: what
/// happened, in one of five words, and what they can do about it.
pub fn publication_view(
    store: &Store,
    review: &crate::store::ReviewRow,
    row: &crate::store::PublicationRow,
) -> serde_json::Value {
    use crate::store::PublicationOutcome as O;
    let outcome = row.outcome(crate::process::instance_id());
    let forge = crate::review::publish::Provider::parse(&row.provider)
        .map(forge_name)
        .unwrap_or("the forge");
    let remedy = match outcome {
        O::NotAttempted => format!(
            "Nothing reached {forge}. Fix what stopped it, then retry the same approved publication."
        ),
        O::Failed => format!(
            "{forge} refused it, or the attempt stopped. A retry sends the same approved commit \
             and description, and checks what {forge} holds before pushing again."
        ),
        O::Uncertain => format!(
            "Tracon could not tell what {forge} holds. A retry looks at {forge} first and only \
             does what has not already happened."
        ),
        O::InProgress | O::Published => String::new(),
    };
    let refusal = match outcome {
        O::NotAttempted | O::Failed | O::Uncertain => recovery_refusal(store, review, row),
        O::InProgress | O::Published => None,
    };
    serde_json::json!({
        "id": row.id,
        "outcome": outcome,
        "note": row.note,
        "url": row.result,
        "attempts": row.attempts,
        "updated_ms": row.updated_ms,
        "provider": row.provider,
        "project": row.project,
        "branch": row.branch,
        "remedy": remedy,
        "recoverable": matches!(outcome, O::NotAttempted | O::Failed | O::Uncertain)
            && refusal.is_none(),
        "refusal": refusal,
    })
}

/// Publish the exact candidate captured by a review. This is shared by the
/// explicit operator approval and policy-authorized publication so neither can
/// skip freshness, the atomic claim, or the brokered SHA precondition.
pub async fn publish_review(
    ctx: &PublishContext<'_>,
    request: PublishRequest<'_>,
) -> Result<Published, PublishError> {
    let PublishRequest {
        review,
        title,
        body,
        require_evidence,
        recheck_authority,
        decided_revision_id,
        outputs: edited_outputs,
        recover,
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
    let submitter = review
        .session_id
        .as_deref()
        .and_then(|id| ctx.store.get_session(id).ok().flatten());
    if let Some(session) = &submitter {
        if session.node_id == ctx.node_id
            && session.harness_id != crate::session::external::HARNESS_ID
            && ctx.manager.snapshot_workspace(&session.id).await.is_err()
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
            // Read again: taking the snapshot above records the worktree.
            review
                .session_id
                .as_deref()
                .and_then(|id| ctx.store.get_session(id).ok().flatten())
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
    let mut target: crate::review::publish::Target =
        serde_json::from_str(&review.target).map_err(|e| PublishError::External(e.to_string()))?;
    let intent =
        revision_intent(ctx.store, revision_id.as_deref()).map_err(PublishError::Conflict)?;
    let outputs = edited_outputs
        .cloned()
        .unwrap_or_else(|| intent.forge.clone())
        .resolve(&target, title, body);
    // The branch and the squashed commit's message are prose like the
    // description: what the operator approved is what ships, held to the
    // same rules the submission was.
    let (_, style) = crate::review::prose::rules(
        ctx.cfg,
        ctx.store,
        &review.channel,
        submitter
            .as_ref()
            .map(|session| std::path::Path::new(&session.repo_path)),
    );
    let mut broken = Vec::new();
    if let Some(branch) = outputs.branch.as_deref().filter(|b| *b != target.branch) {
        if !crate::review::prose::ref_name(branch) {
            return Err(PublishError::Conflict(format!(
                "{branch} is not a branch name git accepts"
            )));
        }
        broken.extend(style.check_branch(branch));
        target.branch = branch.to_string();
    }
    let message = intent
        .squash_onto
        .as_ref()
        .map(|_| crate::review::prose::message_for(&outputs, title, body));
    if let Some(message) = &message {
        broken.extend(style.check_message(message));
    }
    // Under `keep`, each of the agent's commits the operator gave a message
    // ships with it, held to the same rules.
    let reword = intent.squash_onto.is_none() && !outputs.messages.is_empty();
    if reword {
        for (sha, message) in &outputs.messages {
            if !intent.commits.iter().any(|commit| &commit.sha == sha) {
                return Err(PublishError::Conflict(format!(
                    "{sha:.8} is not one of this revision's commits; reload the review and edit \
                     its messages again"
                )));
            }
            broken.extend(
                style
                    .check_message(message)
                    .into_iter()
                    .map(|finding| format!("{sha:.8}: {finding}")),
            );
        }
    }
    if !broken.is_empty() {
        return Err(PublishError::Conflict(format!(
            "what would ship breaks this repository's commit rules: {}",
            broken.join("; ")
        )));
    }
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
        || ((interrupted || recover)
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
            .finish_publish(
                &review.id,
                title,
                body,
                &url,
                &published_target(&target, &url)?,
            )
            .map_err(|e| PublishError::External(e.to_string()))?;
        ctx.manager.publish_queue().await;
        return Ok(Published { url, outputs });
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
    // What this attempt was authorized to send, so a recovery retries these
    // words and no others.
    ctx.store
        .publication_authorized(
            &publication_id,
            title,
            body,
            &serde_json::to_string(&outputs).map_err(|e| PublishError::External(e.to_string()))?,
        )
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
            outputs: &outputs,
            lease: intent.lease.as_deref(),
            rewrite: intent.rewrite,
            squash: intent
                .squash_onto
                .as_deref()
                .zip(message.as_deref())
                .map(|(onto, message)| crate::review::publish::Squash {
                    onto,
                    merges: intent.squash_merges.as_deref(),
                    message,
                }),
            reword: reword.then_some(crate::review::publish::Reword {
                commits: &intent.commits,
                messages: &outputs.messages,
            }),
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
                .finish_publish(
                    &review.id,
                    title,
                    body,
                    &published,
                    &published_target(&target, &published)?,
                )
                .map_err(|e| PublishError::External(e.to_string()))?
            {
                return Err(PublishError::Conflict(
                    "publish completed externally, but the review claim was lost; reconcile it before retrying".into(),
                ));
            }
            ctx.manager.publish_queue().await;
            // The session is answerable for the change now; it says so where
            // the operator looks for it, not only on the review.
            if let Some(session_id) = review.session_id.as_deref() {
                ctx.manager.record_event(
                    session_id,
                    crate::session::state::event_kind::PUBLISHED,
                    serde_json::json!({ "url": published, "review_id": review.id }),
                );
            }
            let submitter = match review.session_id.as_deref() {
                Some(id) => ctx
                    .store
                    .get_session(id)
                    .map_err(|e| PublishError::External(e.to_string()))?,
                None => None,
            };
            if let Some((session_id, item)) =
                submitter.and_then(|s| s.work_item_id.map(|item| (s.id, item)))
            {
                if crate::corpus::work::close(
                    ctx.store,
                    ctx.manager.bus(),
                    ctx.node_id,
                    &item,
                    Some(&session_id),
                )
                .is_ok()
                {
                    ctx.manager
                        .item_closed(&session_id, &format!("published: {published}"))
                        .await;
                }
            }
            Ok(Published {
                url: published,
                outputs,
            })
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
                | crate::review::publish::PublishError::NoReviewedTree
                | crate::review::publish::PublishError::LeaseLost { .. }
                | crate::review::publish::PublishError::SquashBaseGone { .. }
                | crate::review::publish::PublishError::Target(_) => {
                    PublishError::Conflict(error.to_string())
                }
                other => PublishError::External(other.to_string()),
            })
        }
    }
}

/// The target as it published: on the branch it was pushed to, and naming
/// the change it opened, so the review's next revision updates that change.
fn published_target(
    target: &crate::review::publish::Target,
    url: &str,
) -> Result<String, PublishError> {
    serde_json::to_string(&target.clone().opened(url))
        .map_err(|e| PublishError::External(e.to_string()))
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
                session_id: Some("session"),
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
                session_id: Some("session"),
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
