//! The tools the node exposes to a harness, over MCP.
//!
//! The transport is the gateway's forward: the harness can reach the node and
//! nothing else, and it carries a token minted per session. With `[external]`
//! set, a harness the operator runs themselves reaches the same tools through
//! the operator door instead, with no session, on a channel. Either
//! way a tool is the only shape a credential ever reaches the harness in — as
//! something it may ask the node to do, never as something it holds.

pub mod approvals;
pub mod checking;
pub mod consulta;
pub mod docs;
pub mod egress;
pub mod github;
pub mod gitlab;
pub mod jira;
pub mod memory;
pub mod operator;
pub mod parity;
pub mod review;
pub mod schema;
pub mod services;
pub mod setup;
pub mod show;
pub mod wait;
pub mod work;

use std::sync::Arc;

use serde_json::{json, Value};

use crate::{
    adapter::types::OPTION_ALLOW_ONCE,
    broker::SharedBroker,
    config::Config,
    policy::{Decision, Policy, Request, Verdict},
};

/// The policy kind a brokered tool call is evaluated under. Rules with
/// `kinds = ["tool"]` match the tool's name exactly for allow, and any
/// substring of name-plus-arguments for deny.
pub const TOOL_KIND: &str = "tool";

/// The MCP protocol version the node speaks, and announces to a client that
/// names none.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// The revisions this server will serve, newest first. Its whole surface is
/// `initialize`, `tools/list`, `tools/call` and `ping`, which are unchanged
/// across these, so an older client is answered in its own revision rather
/// than being told a newer number it did not ask for. A revision that is not
/// on this list is refused by name: the only client here is a harness the node
/// launched, and one speaking an unknown MCP is a compatibility fault worth
/// seeing rather than a handshake to muddle through.
///
/// `2025-11-25` is what Claude Code 2.1.247 and 2.1.295 announce (observed
/// against the CLI: refusing it is exactly the "failed" MCP server its init
/// frame then reports, and the session never gets its tools).
const SUPPORTED_PROTOCOL_VERSIONS: [&str; 4] =
    ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// How long a held Jira transition waits on Jira to name the status it leads
/// to before it is titled by its id instead.
const TRANSITION_LOOKUP: std::time::Duration = std::time::Duration::from_secs(5);

pub struct Tools {
    pub broker: SharedBroker,
    pub cfg: Arc<Config>,
    /// Every call is decided here before the broker is touched: the same
    /// bundle that answers the harness's own permission requests answers
    /// what the node will do on its behalf. A denied call returns the rule's
    /// reason; one the policy does not cover is put to the operator.
    pub policy: Arc<parking_lot::RwLock<Policy>>,
    /// One client for the forge and tracker tools. Not proxied: the node
    /// reaches those hosts directly; the harness reaches only the node.
    pub http: reqwest::Client,
    /// Set once the manager exists. Review tools need both, and the manager
    /// needs the tools to decide what a session is offered, so the cycle is
    /// broken here rather than by merging the two.
    pub session: std::sync::OnceLock<SessionAccess>,
}

#[derive(Clone)]
pub struct SessionAccess {
    pub store: Arc<crate::store::Store>,
    pub manager: crate::session::Manager,
}

/// What a call knows about who is asking. Channel bindings are enforced here,
/// not in the tool: a tool cannot widen its own reach.
#[derive(Debug, Clone)]
pub struct CallContext {
    pub caller: Caller,
    pub channel: String,
    pub node_id: String,
}

/// Who is calling: a session this node runs, or a harness the operator runs
/// themselves through the external door.
#[derive(Debug, Clone)]
pub enum Caller {
    Session(String),
    External {
        /// What the harness labels itself, for display. Never authority.
        lane: Option<String>,
    },
}

impl CallContext {
    pub fn session(
        session_id: impl Into<String>,
        channel: impl Into<String>,
        node_id: impl Into<String>,
    ) -> Self {
        Self {
            caller: Caller::Session(session_id.into()),
            channel: channel.into(),
            node_id: node_id.into(),
        }
    }

    /// The session this node runs for the call, if the caller is one.
    pub fn session_id(&self) -> Option<&str> {
        match &self.caller {
            Caller::Session(id) => Some(id),
            Caller::External { .. } => None,
        }
    }

    pub fn is_external(&self) -> bool {
        matches!(self.caller, Caller::External { .. })
    }

    pub fn lane(&self) -> Option<&str> {
        match &self.caller {
            Caller::External { lane, .. } => lane.as_deref(),
            Caller::Session(_) => None,
        }
    }

    pub fn external(
        lane: Option<String>,
        channel: impl Into<String>,
        node_id: impl Into<String>,
    ) -> Self {
        Self {
            caller: Caller::External { lane },
            channel: channel.into(),
            node_id: node_id.into(),
        }
    }
}

struct GatedCall {
    one_shot: bool,
}

enum ActionRecord {
    New(String),
    Replay(Value),
}

/// Who a tool definition is offered to: a session the node runs, in its
/// phase, or a harness the operator runs themselves. Each has one list, and
/// `tools/list` and a call are read against the same one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audience {
    Session(crate::session::Phase),
    External,
}

impl From<crate::session::Phase> for Audience {
    fn from(phase: crate::session::Phase) -> Self {
        Self::Session(phase)
    }
}

impl Audience {
    /// Whether this audience is offered `name`, before the channel's
    /// credentials narrow it further.
    pub fn offers(self, name: &str) -> bool {
        use crate::session::Phase;
        match self {
            Self::Session(Phase::Plan) => Tools::PLAN_TOOLS.contains(&name),
            Self::Session(Phase::Execute) => !Tools::NOT_EXECUTE.contains(&name),
            Self::Session(Phase::Review) => Tools::REVIEW_TOOLS.contains(&name),
            Self::External => !Tools::NOT_EXTERNAL.contains(&name),
        }
    }

    /// Why a call to a tool this audience is not offered is refused.
    fn refusal(self, name: &str) -> String {
        use crate::session::Phase;
        match self {
            Self::Session(Phase::Review) => format!(
                "{name} is not offered in a review session; give a verdict with {}",
                review::VERDICT
            ),
            Self::Session(phase) => {
                let article = if phase == Phase::Execute { "an" } else { "a" };
                let refusal = format!(
                    "{name} is not offered in {article} {} session",
                    phase.as_str()
                );
                if name == review::FINDING {
                    format!(
                        "{refusal}: only a review session records a finding; {} is how an \
                         execute session says what would settle a criterion",
                        work::CRITERIA_LINK
                    )
                } else if name == review::VERDICT {
                    format!(
                        "{refusal}: only a review session gives a verdict; an execute session \
                         calls {}",
                        review::SUBMIT
                    )
                } else {
                    refusal
                }
            }
            Self::External if name == review::VERDICT => format!(
                "{name} is for a review session the node starts; put your own change up with {}",
                review::SUBMIT
            ),
            Self::External => format!(
                "{name} is not offered to a harness you run yourself: it works on the item a \
                 session holds, and yours holds none"
            ),
        }
    }
}

impl Tools {
    /// The tools a plan session gets: what it needs to read, ask and write its
    /// plan, and to record work it finds. Nothing that closes the item,
    /// submits, sets anything up or writes to a forge or tracker; the reading
    /// forge and Jira tools are here, and the channel's credentials decide
    /// which of those it actually has.
    pub const PLAN_TOOLS: &'static [&'static str] = &[
        memory::RECALL,
        memory::RETAIN,
        memory::AMEND,
        docs::DOC_READ,
        docs::DOC_SEARCH,
        docs::DOC_WRITE,
        work::BRIEF_READ,
        work::CRITERIA_READ,
        operator::ASK,
        operator::QUESTION_STATUS,
        operator::NOTIFY,
        work::WORK_READY,
        work::WORK_DISCOVER,
        work::WORK_LIST,
        work::WORK_SHOW,
        github::PR_STATUS,
        github::PR_THREADS,
        github::PR_FOR_BRANCH,
        github::RUN_STATUS,
        github::RUN_LOGS,
        github::RUN_WAIT,
        gitlab::MR_STATUS,
        gitlab::MR_DISCUSSIONS,
        gitlab::MR_FOR_BRANCH,
        gitlab::PIPELINE_STATUS,
        gitlab::PIPELINE_WAIT,
        gitlab::PIPELINE_LIST_BY_SHA,
        gitlab::JOB_TRACE,
        jira::ISSUE,
        jira::ISSUE_SEARCH,
        jira::ISSUE_TRANSITIONS,
    ];

    /// What an execute session is not offered: everything else is its to use.
    /// A verdict and findings are a review session's account of someone
    /// else's change.
    pub const NOT_EXECUTE: &'static [&'static str] = &[review::VERDICT, review::FINDING];

    /// The tools a review session gets: what it needs to read, what it needs
    /// to exercise the candidate and show what it saw, its findings, and its
    /// verdict. Nothing that publishes or reaches a forge.
    pub const REVIEW_TOOLS: &'static [&'static str] = &[
        memory::RECALL,
        docs::DOC_READ,
        docs::DOC_SEARCH,
        // What good was agreed to be, where the item said so. A reviewer
        // asked to judge against criteria it cannot read is judging its own.
        work::BRIEF_READ,
        // And which of those criteria anything actually points at: a reviewer
        // that cannot see an uncovered criterion cannot say it is uncovered.
        work::CRITERIA_READ,
        // What it thinks would settle a criterion, asked like any session's.
        work::CRITERIA_LINK,
        // The item under review and its neighbours, read-only.
        work::WORK_LIST,
        work::WORK_SHOW,
        // Running the candidate where no check settles a criterion: a
        // browser beside the session, decided by the policy bundle by name,
        // and what it saw shown to the operator, bound to the candidate.
        services::START,
        services::STATUS,
        show::SHOW,
        // Per criterion, what running it showed: never a verdict on one.
        review::FINDING,
        review::VERDICT,
    ];

    /// What an external harness is not offered: a verdict and findings are
    /// what a review session the node started gives on someone else's change,
    /// and the brief
    /// and criteria tools read and write the item a session holds, which an
    /// external harness never does.
    pub const NOT_EXTERNAL: &'static [&'static str] = &[
        review::VERDICT,
        review::FINDING,
        work::BRIEF_READ,
        work::BRIEF_NOTE,
        work::CRITERIA_READ,
        work::CRITERIA_LINK,
    ];

    /// Tool definitions for a channel, narrowed to what `audience` is offered.
    /// This is the one list: `tools/list`, the orientation, the session's
    /// wiring and its explanation all read it.
    pub fn list_for(
        &self,
        channel: &str,
        node_id: &str,
        audience: impl Into<Audience>,
    ) -> Vec<Value> {
        let audience = audience.into();
        self.list(channel, node_id)
            .into_iter()
            .filter(|t| t["name"].as_str().is_some_and(|n| audience.offers(n)))
            .collect()
    }

    /// Tool definitions for a channel. A channel with no credential bound to it
    /// is offered no tools rather than tools that will fail.
    pub fn list(&self, channel: &str, node_id: &str) -> Vec<Value> {
        let mut out = Vec::new();
        let broker = self.broker.read().unwrap();
        let available = broker.available_to(channel, node_id);
        let profiles = consulta::profiles(&available);
        if !profiles.is_empty() {
            out.extend(consulta::definitions(&profiles));
        }
        if available.contains(&gitlab::CREDENTIAL) {
            out.extend(gitlab::definitions());
        }
        if available.contains(&github::CREDENTIAL) {
            out.extend(github::definitions());
        }
        if available.contains(&jira::CREDENTIAL) {
            out.extend(jira::definitions());
        }
        // Review tools need no credential of their own: submitting is always
        // allowed, and publishing is what needs one. An agent that cannot
        // publish can still ask for review and be told why it stopped there.
        if self.session.get().is_some() {
            let browser = self.cfg.service.iter().any(|s| s.name == services::BROWSER);
            out.extend(review::definitions(browser));
            // Memory and documents need no credential either: the corpus is
            // the node's own, and reading it is what every session starts by
            // doing. This means every session gets an MCP server.
            out.extend(memory::definitions());
            out.extend(docs::definitions(&self.cfg.live().docs.kind));
            // These are intervention-only: asking, pinging, and drafting a
            // report do not touch a credential or widen a tool policy.
            out.extend(operator::definitions());
            // Asking for a host only asks: the operator's answer is what
            // opens anything, and it opens only this session's own grant.
            out.extend(egress::definitions());
            // Showing work reaches the operator and nothing else: it is read
            // from the node's own snapshot and never leaves tracon.
            out.extend(show::definitions());
            // Drafting and trying a repository's entry reach nothing this
            // session cannot; proposing one is always the operator's call.
            out.extend(setup::definitions());
            out.extend(work::definitions());
            out.extend(approvals::definitions());
            // Only what the operator's catalogue holds, and only by name:
            // the policy bundle decides each service on its own.
            if !self.cfg.service.is_empty() {
                out.extend(services::definitions(&self.cfg));
            }
        }
        out
    }

    pub async fn call(&self, ctx: &CallContext, name: &str, args: &Value) -> Result<Value, String> {
        let defaulted = self.with_defaults(ctx, name, args)?;
        let args = defaulted.as_ref().unwrap_or(args);
        // A plan session's own plan document is the phase's artifact: writing
        // that one slug is what the session exists to do, so it is not asked.
        // A delete, and a write that converts a document or drops files of
        // its bundle, are always the operator's, whatever the policy says;
        // so is an amendment, which rewrites a memory in place with no
        // nightly batch behind it.
        let always_asked = name == docs::DOC_DELETE
            || name == memory::AMEND
            || name == docs::DOC_WRITE
                && self
                    .session
                    .get()
                    .and_then(|access| docs::write_consequence(&access.store, &ctx.channel, args))
                    .is_some();
        let plan_write =
            name == docs::DOC_WRITE && !always_asked && self.is_plan_artifact(ctx, args);
        let audience = self.audience(ctx);
        if !audience.offers(name) {
            return Err(audience.refusal(name));
        }
        // A directive, another channel's memory, or one no longer live is
        // refused before it is held, as a correction's target is.
        if name == memory::AMEND {
            if let Some(access) = self.session.get() {
                memory::amend_target(&access.store, &ctx.channel, args)?;
            }
        }
        if matches!(
            name,
            operator::ASK
                | operator::NOTIFY
                | operator::REPORT
                | operator::REPORT_STATUS
                | operator::QUESTION_STATUS
        ) {
            let access = self
                .session
                .get()
                .ok_or("operator interventions are not available on this node")?;
            return operator::call(&access.store, &access.manager, &self.cfg, ctx, name, args)
                .await;
        }
        if name == egress::REQUEST {
            let access = self
                .session
                .get()
                .ok_or("egress requests are not available on this node")?;
            access
                .manager
                .caller_active(ctx)
                .map_err(|error| error.to_string())?;
            return egress::call(access, ctx, args).await;
        }
        if matches!(name, setup::DRAFT | setup::TRY | setup::PROPOSE) {
            let access = self
                .session
                .get()
                .ok_or("repository setup is not available on this node")?;
            access
                .manager
                .caller_active(ctx)
                .map_err(|error| error.to_string())?;
            if name == setup::PROPOSE {
                // Always the operator's, whatever the policy says: an agent
                // does not write its own repository's environment.
                let asked = setup::proposal(access, ctx, args)?;
                return self.request_approval(ctx, name, &asked).await;
            }
            if name == setup::TRY {
                // Building a repository's Dockerfile runs it with the network,
                // outside the boundary: the operator's, as when they name one
                // in an entry themselves.
                if let Some(asked) = setup::build_to_ask(access, ctx, args).await? {
                    return self.request_approval(ctx, name, &asked).await;
                }
            }
            return setup::call(access, ctx, name, args).await;
        }
        if name == show::SHOW {
            let access = self
                .session
                .get()
                .ok_or("showing work is not available on this node")?;
            access
                .manager
                .caller_active(ctx)
                .map_err(|error| error.to_string())?;
            return show::call(access, ctx, args).await;
        }
        if name == approvals::STATUS {
            let access = self
                .session
                .get()
                .ok_or("approvals are not available on this node")?;
            return approvals::status(access, ctx, args).await;
        }
        if consequential_name(name) && consequential(name, args).is_none() {
            return Err(
                "consequential calls require a valid operation_id and canonical arguments".into(),
            );
        }
        // A workflow the node does not start is refused before the operator
        // is asked about it, not after.
        if let Some(refusal) = self.refusal_before_asking(name, args) {
            return Err(refusal);
        }
        if !plan_write {
            let decision = self.decide(ctx, name, args);
            match decision.verdict {
                Verdict::Allow if !always_asked => {}
                Verdict::Deny => return Err(refusal(decision)),
                // Nothing waits on the operator: the call is held as an
                // approval and the caller is told so at once.
                Verdict::Allow | Verdict::Ask => {
                    let pinned = match name {
                        docs::DOC_WRITE | docs::DOC_DELETE => self
                            .session
                            .get()
                            .and_then(|access| docs::pinned(&access.store, &ctx.channel, args)),
                        _ => None,
                    };
                    return self
                        .request_approval(ctx, name, pinned.as_ref().unwrap_or(args))
                        .await;
                }
            }
        }
        if let Some(access) = self.session.get() {
            access
                .manager
                .caller_active(ctx)
                .map_err(|error| error.to_string())?;
        }
        self.dispatch(ctx, name, args, false).await
    }

    /// Run a call that is allowed: by policy, by a grant, or by the operator
    /// (`one_shot`). Everything a call is checked against is checked again
    /// here, because an approved call runs long after it was asked.
    async fn dispatch(
        &self,
        ctx: &CallContext,
        name: &str,
        args: &Value,
        one_shot: bool,
    ) -> Result<Value, String> {
        // The operator may have edited the arguments in the prompt; a default
        // they removed is put back the same visible way.
        let defaulted = self.with_defaults(ctx, name, args)?;
        let args = defaulted.as_ref().unwrap_or(args);
        let gated = GatedCall { one_shot };
        if consequential_name(name) && consequential(name, args).is_none() {
            return Err(
                "consequential calls require a valid operation_id and canonical arguments".into(),
            );
        }
        if let Some(refusal) = self.refusal_before_asking(name, args) {
            return Err(refusal);
        }
        self.revalidate_consequential(ctx, name, args, gated.one_shot)?;
        let action_record = self.begin_consequential(ctx, name, args, gated.one_shot)?;
        if let Some(ActionRecord::Replay(outcome)) = &action_record {
            return Ok(outcome.clone());
        }
        let before_mutation = || {
            let Some(ActionRecord::New(id)) = &action_record else {
                return Ok(());
            };
            let (action, target, revision) = consequential(name, args)
                .expect("consequential action record has canonical arguments");
            let policy = self.policy.read();
            let live = self.cfg.live();
            let tool = policy.decide(&Request {
                channel: &ctx.channel,
                action: name,
                kind: Some(TOOL_KIND),
                resource: None,
                command: None,
                arguments: Some(args),
                doc_kinds: Some(&live.docs.kind),
            });
            if tool.verdict == Verdict::Deny {
                return Err(refusal(tool));
            }
            let access = self.session.get().ok_or("authority requires a session")?;
            let decision = crate::authority::decide(
                access.store.as_ref(),
                &policy,
                &crate::authority::AuthorityQuery {
                    channel: &ctx.channel,
                    session_id: ctx.session_id(),
                    action,
                    target: &target,
                    revision: revision.as_deref(),
                    args,
                },
            )?;
            match decision.verdict {
                Verdict::Allow => {}
                Verdict::Ask if gated.one_shot => {}
                Verdict::Deny => return Err(refusal(decision)),
                Verdict::Ask => {
                    return Err(format!(
                        "{action} for {target} needs a current scoped authority grant"
                    ))
                }
            }
            access
                .store
                .authority_action_set_grant(id, decision.rule_id.as_deref())
                .map_err(|e| e.to_string())
        };
        let result = match name {
            consulta::QUERY | consulta::DESCRIBE => {
                consulta::call(&self.broker, &self.cfg, ctx, name, args).await
            }
            gitlab::MR_STATUS
            | gitlab::MR_COMMENT
            | gitlab::MR_MERGE
            | gitlab::MR_READY
            | gitlab::PIPELINE_STATUS
            | gitlab::PIPELINE_WAIT
            | gitlab::PIPELINE_LIST_BY_SHA
            | gitlab::JOB_TRACE
            | gitlab::JOB_PLAY
            | gitlab::PIPELINE_RUN
            | gitlab::DEPLOY
            | gitlab::MR_DISCUSSIONS
            | gitlab::MR_REPLY
            | gitlab::MR_FOR_BRANCH
            | gitlab::PIPELINE_RETRY => {
                gitlab::call(
                    &self.broker,
                    &self.http,
                    ctx,
                    name,
                    args,
                    Some(&before_mutation),
                )
                .await
            }
            jira::ISSUE
            | jira::ISSUE_SEARCH
            | jira::ISSUE_TRANSITIONS
            | jira::ISSUE_COMMENT
            | jira::ISSUE_UPDATE
            | jira::ISSUE_CREATE
            | jira::ISSUE_TRANSITION => {
                jira::call(
                    &self.broker,
                    &self.http,
                    ctx,
                    name,
                    args,
                    Some(&before_mutation),
                )
                .await
            }
            review::SUBMIT
            | review::STATUS
            | review::SUBMIT_REPORT
            | review::REPORT_STATUS
            | review::VERDICT
            | review::FINDING => {
                let access = self
                    .session
                    .get()
                    .ok_or("review tools are not available on this node")?;
                review::call(&access.store, &access.manager, ctx, name, args).await
            }
            memory::RECALL | memory::RETAIN => {
                let access = self
                    .session
                    .get()
                    .ok_or("memory is not available on this node")?;
                memory::call(self, access, ctx, name, args).await
            }
            memory::AMEND => {
                if !gated.one_shot {
                    return Err(format!("{name} runs only once the operator allows it"));
                }
                let access = self
                    .session
                    .get()
                    .ok_or("memory is not available on this node")?;
                memory::amend(access, ctx, args)
            }
            setup::PROPOSE => {
                if !gated.one_shot {
                    return Err(format!(
                        "{name} is written only once the operator allows it"
                    ));
                }
                let access = self
                    .session
                    .get()
                    .ok_or("repository setup is not available on this node")?;
                setup::write(access, ctx, args)
            }
            setup::TRY => {
                if !gated.one_shot {
                    return Err(format!(
                        "{name} builds an image only once the operator allows it"
                    ));
                }
                let access = self
                    .session
                    .get()
                    .ok_or("repository setup is not available on this node")?;
                setup::start_allowed(access, ctx, args)
            }
            work::WORK_READY
            | work::WORK_DISCOVER
            | work::WORK_CLOSE
            | work::WORK_LIST
            | work::WORK_SHOW
            | work::WORK_UPDATE
            | work::WORK_DELETE
            | work::BRIEF_READ
            | work::BRIEF_NOTE
            | work::CRITERIA_READ
            | work::CRITERIA_LINK => {
                let access = self
                    .session
                    .get()
                    .ok_or_else(|| "node not ready".to_string())?;
                work::call(&self.cfg, access, ctx, name, args).await
            }
            docs::DOC_READ | docs::DOC_SEARCH | docs::DOC_WRITE | docs::DOC_DELETE => {
                let access = self
                    .session
                    .get()
                    .ok_or("documents are not available on this node")?;
                docs::call(self, access, ctx, name, args).await
            }
            github::PR_STATUS
            | github::PR_COMMENT
            | github::ISSUE_COMMENT
            | github::RUN_STATUS
            | github::PR_MERGE
            | github::PR_READY
            | github::PR_THREADS
            | github::PR_REPLY
            | github::PR_FOR_BRANCH
            | github::RUN_LOGS
            | github::RUN_RERUN
            | github::RUN_WAIT
            | github::RUN_DISPATCH => {
                github::call(
                    &self.broker,
                    &self.http,
                    ctx,
                    name,
                    args,
                    Some(&before_mutation),
                )
                .await
            }
            services::START | services::STATUS => {
                let access = self
                    .session
                    .get()
                    .ok_or("services are not available on this node")?;
                services::call(access, ctx, name, args).await
            }
            other => Err(format!("no tool named {other}")),
        };
        let result = match (name, result) {
            (
                gitlab::PIPELINE_RUN
                | gitlab::JOB_PLAY
                | gitlab::DEPLOY
                | gitlab::PIPELINE_RETRY
                | github::RUN_RERUN
                | github::RUN_DISPATCH,
                Ok(started),
            ) => Ok(crate::follow::subscribe(self, ctx, name, args, started).await),
            (_, result) => result,
        };
        if let Some(ActionRecord::New(id)) = action_record {
            let (state, outcome) = match &result {
                Ok(value) => ("succeeded", value.to_string()),
                Err(error) if remote_outcome_unknown(error) => ("uncertain", error.clone()),
                Err(error) => ("failed", error.clone()),
            };
            self.session
                .get()
                .expect("authority record requires session")
                .store
                .authority_action_finish(&id, state, &outcome)
                .map_err(|e| e.to_string())?;
        }
        result
    }

    /// Publish a review `submit_review` just recorded when an authority grant
    /// covers it. Called by the submission itself, which for a session is the
    /// node's check task once the checks pass rather than this call.
    pub(crate) async fn auto_publish_review(
        &self,
        ctx: &CallContext,
        submitted: Value,
    ) -> Result<Value, String> {
        let Some(review_id) = submitted.get("review_id").and_then(Value::as_str) else {
            return Ok(submitted);
        };
        let access = self.session.get().ok_or("review tools are not available")?;
        let review = access
            .store
            .get_review(review_id)
            .map_err(|e| e.to_string())?
            .ok_or("review disappeared after capture")?;
        if review.kind == crate::store::reports::KIND {
            return Err("standalone narrative reports have no automatic publication path".into());
        }
        // The revision the authority decision below is computed against.
        // Authority is granted for a revision, so the publication is bound to
        // that same one: a resubmission arriving while this runs does not
        // inherit the grant.
        let decided_revision = access
            .store
            .latest_review_revision(review_id)
            .map_err(|e| e.to_string())?
            .map(|revision| revision.id);
        let target: crate::review::publish::Target =
            serde_json::from_str(&review.target).map_err(|e| e.to_string())?;
        let intent =
            crate::authority::revision_intent(access.store.as_ref(), decided_revision.as_deref())?;
        // The prose hash binds everything the forge will show. A revision
        // that asks for nothing beyond a new change described by its title
        // and body hashes exactly as it always has, so existing grants hold.
        let prose = if intent.forge == Default::default() {
            crate::corpus::hash_body(&format!(
                "{}\u{1f}{}",
                review.approved_title(),
                review.approved_body()
            ))
        } else {
            crate::corpus::hash_body(&format!(
                "{}\u{1f}{}\u{1f}{}",
                review.approved_title(),
                review.approved_body(),
                serde_json::to_string(&intent.forge).map_err(|e| e.to_string())?
            ))
        };
        let change = target
            .change
            .as_ref()
            .map(|change| format!(":change:{}", change.number))
            .unwrap_or_default();
        let canonical = format!(
            "publish:{}:{}:{}:{}{change}:prose:{}",
            target.provider, target.project, target.base, target.branch, prose
        );
        let authority_args = serde_json::json!({
            "target": canonical.clone(),
            "revision": review.head_sha.clone(),
            "prose_hash": prose.clone(),
        });
        let mut decision = crate::authority::decide(
            access.store.as_ref(),
            &self.policy.read(),
            &crate::authority::AuthorityQuery {
                channel: &ctx.channel,
                session_id: ctx.session_id(),
                action: crate::authority::PUBLISH,
                target: authority_args["target"].as_str().expect("canonical target"),
                revision: authority_args["revision"].as_str(),
                args: &authority_args,
            },
        )?;
        // Replacing history on a shared branch is never unattended, whatever
        // a grant says: the operator sees what it throws away.
        if intent.rewrite && decision.verdict == Verdict::Allow {
            decision.verdict = Verdict::Ask;
            decision.reason =
                Some("a push that rewrites a change's history always goes to the operator".into());
        }
        if decision.verdict != Verdict::Allow {
            let mut submitted = submitted;
            if let Some(object) = submitted.as_object_mut() {
                object.insert(
                    "publication_authority".into(),
                    serde_json::json!({
                        "action": crate::authority::PUBLISH,
                        "target": canonical,
                        "revision": review.head_sha,
                        "prose_hash": prose,
                        "state": match decision.verdict {
                            Verdict::Allow => "allow",
                            Verdict::Ask => "ask",
                            Verdict::Deny => "deny",
                        },
                        "reason": decision.reason,
                    }),
                );
            }
            return Ok(submitted);
        }
        let operation_id =
            crate::corpus::hash_body(&format!("{canonical}\u{1f}{}", review.head_sha));
        let evidence = serde_json::json!({
            "target": canonical.clone(),
            "revision": review.head_sha.clone(),
            "prose_hash": prose.clone(),
        })
        .to_string();
        if let Some((state, outcome, _)) = access
            .store
            .authority_action_existing(
                crate::authority::PUBLISH,
                &canonical,
                &ctx.channel,
                Some(&review.head_sha),
                &operation_id,
            )
            .map_err(|e| e.to_string())?
        {
            if state == "succeeded" {
                return Ok(serde_json::json!({
                    "review_id": review.id,
                    "state": "reconcile",
                    "authority": {
                        "mode": "automatic",
                        "outcome": "replayed",
                        "published": outcome,
                    },
                }));
            }
            return Err(format!(
                "an identical publication is {state}; reconcile its recorded external outcome before retrying"
            ));
        }
        let action_id = uuid::Uuid::now_v7().to_string();
        if let Err(error) = access
            .store
            .authority_action_begin(&crate::store::ActionBegin {
                id: &action_id,
                grant_id: decision.rule_id.as_deref(),
                action: crate::authority::PUBLISH,
                target: &canonical,
                channel: &ctx.channel,
                session_id: ctx.session_id(),
                revision: Some(&review.head_sha),
                operation_id: Some(&operation_id),
                evidence: &evidence,
            })
        {
            if let Some((state, outcome, _)) = access
                .store
                .authority_action_existing(
                    crate::authority::PUBLISH,
                    &canonical,
                    &ctx.channel,
                    Some(&review.head_sha),
                    &operation_id,
                )
                .map_err(|e| e.to_string())?
            {
                if state == "succeeded" {
                    return Ok(serde_json::json!({
                        "review_id": review.id,
                        "state": "reconcile",
                        "authority": {
                            "mode": "automatic",
                            "outcome": "replayed",
                            "published": outcome,
                        },
                    }));
                }
                return Err(format!(
                    "an identical publication is {state}; reconcile its recorded external outcome before retrying"
                ));
            }
            return Err(error.to_string());
        }
        let recheck = || {
            let current = crate::authority::decide(
                access.store.as_ref(),
                &self.policy.read(),
                &crate::authority::AuthorityQuery {
                    channel: &ctx.channel,
                    session_id: ctx.session_id(),
                    action: crate::authority::PUBLISH,
                    target: &canonical,
                    revision: Some(&review.head_sha),
                    args: &authority_args,
                },
            )?;
            if current.verdict != Verdict::Allow {
                return Err("publication authority was revoked or no longer applies".into());
            }
            access
                .store
                .authority_action_set_grant(&action_id, current.rule_id.as_deref())
                .map_err(|e| e.to_string())
        };
        match crate::authority::publish_review(
            &crate::authority::PublishContext {
                store: access.store.as_ref(),
                manager: &access.manager,
                broker: &self.broker,
                cfg: &self.cfg,
                node_id: &ctx.node_id,
            },
            crate::authority::PublishRequest {
                review: &review,
                title: review.approved_title(),
                body: review.approved_body(),
                require_evidence: true,
                recheck_authority: Some(&recheck),
                decided_revision_id: decided_revision.as_deref(),
                outputs: None,
                recover: false,
            },
        )
        .await
        {
            Ok(crate::authority::Published {
                url: published,
                outputs,
            }) => {
                access
                    .store
                    .authority_action_finish(&action_id, "succeeded", &published)
                    .map_err(|e| e.to_string())?;
                // Candidate evidence records who authorized publication even
                // when no operator ever saw the card: the automatic caller
                // gets its own decision source, and never the operator's
                // `review_decision` event (that is a human-waiting metric).
                if let Some(revision) = access
                    .store
                    .latest_review_revision(&review.id)
                    .map_err(|e| e.to_string())?
                {
                    access
                        .store
                        .record_review_decision(&crate::store::ReviewDecisionRow {
                            id: uuid::Uuid::now_v7().to_string(),
                            review_id: review.id.clone(),
                            revision_id: revision.id,
                            source: "authority".into(),
                            decision: "approved".into(),
                            reason: None,
                            title: Some(review.approved_title().to_string()),
                            body: Some(review.approved_body().to_string()),
                            patch: None,
                            decided_ms: crate::store::now_ms(),
                            outputs_json: serde_json::to_string(&outputs).ok(),
                        })
                        .map_err(|e| e.to_string())?;
                }
                Ok(serde_json::json!({
                    "review_id": review.id, "state": "approved", "published": published,
                    "authority": { "mode": "automatic", "id": decision.rule_id },
                }))
            }
            Err(error) => {
                // A publication whose outcome could not be established is
                // `uncertain`, not `failed`: the replay guard keeps holding
                // the scope, and nobody is told the change was not opened.
                let outcome = match error {
                    crate::authority::PublishError::Uncertain(_) => "uncertain",
                    _ => "failed",
                };
                let error = error.to_string();
                access
                    .store
                    .authority_action_finish(&action_id, outcome, &error)
                    .map_err(|e| e.to_string())?;
                let state = access
                    .store
                    .get_review(&review.id)
                    .map_err(|e| e.to_string())?
                    .map(|current| current.state)
                    .unwrap_or_else(|| "missing".into());
                Ok(serde_json::json!({
                    "review_id": review.id, "state": state,
                    "authority": { "mode": "automatic", "outcome": outcome, "reason": error },
                }))
            }
        }
    }

    /// The definitions this caller is offered: the channel's tools, as
    /// [`Self::list_for`] narrows them to its phase. A session on a forge
    /// repository is not required to name it to that forge's tools, since the
    /// call defaults to it; an external caller has no repository to default to.
    pub fn list_offered(&self, ctx: &CallContext) -> Vec<Value> {
        let mut all = self.list_for(&ctx.channel, &ctx.node_id, self.audience(ctx));
        if !ctx.is_external() {
            if let Some(repo) = self.session_forge_repo(ctx) {
                for tool in &mut all {
                    let defaulted: &[&str] = match tool["name"].as_str() {
                        Some(review::SUBMIT) => &["provider", "project"],
                        Some(name) if forge_of_tool(name) == Some(repo.forge) => &[repo.argument()],
                        _ => &[],
                    };
                    // `get_mut`, not indexing: `tool[..]["required"]` on a
                    // schema without one inserts `null`, and Claude Code
                    // drops every tool of a server offering one.
                    if let Some(required) = tool
                        .get_mut("inputSchema")
                        .and_then(|schema| schema.get_mut("required"))
                        .and_then(Value::as_array_mut)
                    {
                        required.retain(|key| !defaulted.iter().any(|d| key == d));
                    }
                }
            }
        }
        all
    }

    /// The repository the calling session works on, as its launch resolved
    /// it from `origin` into the project row: two row reads, and no git. `None`
    /// for an external caller, and for a session on a local-only repository.
    fn session_forge_repo(&self, ctx: &CallContext) -> Option<crate::forge::ForgeRepo> {
        let id = ctx.session_id()?;
        let remote = self.session.get()?.store.session_remote(id).ok()??;
        crate::forge::ForgeRepo::from_remote(&remote)
    }

    /// The arguments with every default written in: the provider's own, and
    /// the calling session's repository for its forge's tools and for
    /// `submit_review`. Applied before policy, approval, authority and any
    /// hash of the arguments, so a call that leaves a default out and one
    /// that spells it are the same request. `None` when nothing was missing.
    fn with_defaults(
        &self,
        ctx: &CallContext,
        name: &str,
        args: &Value,
    ) -> Result<Option<Value>, String> {
        if name == review::SUBMIT {
            return submission_defaults(args, self.session_forge_repo(ctx).as_ref());
        }
        // Only a tool on the session's own forge can take its repository.
        let repo = forge_of_tool(name).and_then(|forge| {
            self.session_forge_repo(ctx)
                .filter(|repo| repo.forge == forge)
        });
        Ok(with_provider_defaults(name, args, repo.as_ref()))
    }

    /// What the node's own configuration refuses whatever the policy or the
    /// operator says: a workflow `[github] dispatch` does not list, a
    /// `ready` that is not true or false, or a document call that cannot run
    /// (`docs::refusal_before_asking`).
    fn refusal_before_asking(&self, name: &str, args: &Value) -> Option<String> {
        match name {
            docs::DOC_WRITE | docs::DOC_DELETE => docs::refusal_before_asking(name, args),
            github::RUN_DISPATCH => github::dispatch_refusal(&self.cfg.live().github, args),
            // The card names the direction, so a `ready` it cannot read is
            // refused before it is shown.
            github::PR_READY | gitlab::MR_READY => github::ready_arg(args).err(),
            _ => None,
        }
    }

    /// Whose list the caller is read against: its session's phase as the
    /// session's row records it, or the external harness's. A session the
    /// store does not know is read as ordinary work.
    fn audience(&self, ctx: &CallContext) -> Audience {
        use crate::session::Phase;
        let Some(id) = ctx.session_id() else {
            return Audience::External;
        };
        let phase = self
            .session
            .get()
            .and_then(|a| a.store.get_session(id).ok().flatten())
            .map(|s| Phase::from_row(&s.phase))
            .unwrap_or_default();
        Audience::Session(phase)
    }

    fn is_plan_artifact(&self, ctx: &CallContext, args: &Value) -> bool {
        let (Some(access), Some(id)) = (self.session.get(), ctx.session_id()) else {
            return false;
        };
        let Ok(Some(session)) = access.store.get_session(id) else {
            return false;
        };
        session.phase == "plan"
            && session.work_item_id.as_deref().is_some_and(|item| {
                args["slug"].as_str().map(str::trim)
                    == Some(crate::corpus::work::plan_slug(item).as_str())
            })
    }

    /// Policy before the broker. Deny is final and explained; Ask goes to the
    /// queue as a permission request on the calling session and waits for the
    /// operator (or the same expiry every unanswered request gets); Allow
    /// proceeds. A tool the policy does not mention is therefore asked, not
    /// run — adding a tool never widens what runs unattended. Returns the
    /// arguments the operator rewrote on the card, when they did.
    /// Hold a call the operator must decide as an approval and say so. Asking
    /// again for the same thing on the channel while it waits is the same
    /// approval, so a client that retries does not queue a second card.
    async fn request_approval(
        &self,
        ctx: &CallContext,
        name: &str,
        args: &Value,
    ) -> Result<Value, String> {
        use crate::store::approvals::{ApprovalRow, PENDING};
        let access = self
            .session
            .get()
            .ok_or("this call needs the operator's approval and this node cannot hold one")?;
        let request_key = request_key(name, args);
        let existing = access
            .store
            .pending_approval(&ctx.channel, &request_key)
            .map_err(|e| e.to_string())?;
        let approval = match existing {
            Some(approval) => approval,
            None => {
                let title = self.approval_title(ctx, name, args).await;
                let now = crate::store::now_ms();
                let row = ApprovalRow {
                    id: uuid::Uuid::now_v7().to_string(),
                    channel: ctx.channel.clone(),
                    session_id: ctx.session_id().map(str::to_string),
                    node_id: ctx.node_id.clone(),
                    lane: ctx.lane().map(str::to_string),
                    tool: name.to_string(),
                    arguments: args.to_string(),
                    request_key,
                    title,
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
                    expires_ms: now
                        + (self.cfg.live().session.approval_expiry_secs.max(1) as i64) * 1000,
                };
                access
                    .store
                    .insert_approval(&row)
                    .map_err(|e| e.to_string())?;
                access.manager.approval_requested(&row).await;
                row
            }
        };
        Ok(json!({
            "state": "awaiting_operator",
            "approval_id": approval.id,
            "summary": approval.title,
            "message": format!(
                "Nothing has run yet. The operator decides this call; once allowed, the node \
                 runs it and keeps the result. Call {} with this approval_id to wait for the \
                 outcome.",
                approvals::STATUS
            ),
        }))
    }

    /// What the operator reads on the card and in the push for a held call:
    /// [`summarize`], except that a Jira transition is named by the status it
    /// leads to, which only Jira can say, and a document write says whether
    /// it creates or edits, and what, and when it changes the document's
    /// format or drops files of its bundle. A lookup that fails or is slow
    /// leaves the plain summary rather than holding the request.
    async fn approval_title(&self, ctx: &CallContext, name: &str, args: &Value) -> String {
        if name == jira::ISSUE_TRANSITION {
            let lookup = jira::transition_target(&self.broker, &self.http, ctx, args);
            if let Ok(Some(to)) = tokio::time::timeout(TRANSITION_LOOKUP, lookup).await {
                return transition_title(args, &to);
            }
        }
        if name == docs::DOC_WRITE {
            if let Some(access) = self.session.get() {
                if let Ok(current) = docs::replaced(&access.store, &ctx.channel, args) {
                    let mut title = docs::approval_title(&summarize(name, args), current.as_ref());
                    if let Some(consequence) =
                        docs::write_consequence(&access.store, &ctx.channel, args)
                    {
                        title = format!("{title} ({consequence})");
                    }
                    bound(&mut title);
                    return title;
                }
            }
        }
        summarize(name, args)
    }

    /// The operator's answer to an approval. Allowing it runs the call, with
    /// their edit when they made one, held to every refusal the call itself
    /// would meet. Refusing it, or asking for changes, settles it with what
    /// they said, for the caller to read. The answer returns once the
    /// decision is recorded; the run and its result belong to the approval,
    /// not to whoever answered.
    pub async fn answer_approval(
        self: &Arc<Self>,
        id: &str,
        answer: crate::session::OperatorAnswer,
    ) -> Result<(), crate::session::SessionError> {
        use crate::session::SessionError;
        use crate::store::approvals::{
            Decision, CHANGES_REQUESTED, OPTION_REQUEST_CHANGES, PENDING, REJECTED, RUNNING,
        };
        let rejected = |m: &str| SessionError::Rejected(m.to_string());
        let access = self
            .session
            .get()
            .ok_or_else(|| rejected("approvals are not available"))?;
        let approval = access
            .store
            .get_approval(id)?
            .ok_or_else(|| rejected("no such approval"))?;
        let no_longer_waiting = || rejected("this approval is no longer waiting");
        if approval.state != PENDING {
            return Err(no_longer_waiting());
        }
        // Expired is over even before the sweep marks it so. Checked here,
        // ahead of every answer, because some answers write before they
        // decide (saving an egress host to the repository).
        if approval.expires_ms <= crate::store::now_ms() {
            return Err(rejected("this approval has expired"));
        }
        let said = |text: &Option<String>| {
            text.as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string)
        };
        let (reason, notes) = (said(&answer.reason), said(&answer.notes));
        let option_id = answer.option_id.as_str();
        if approval.tool == egress::REQUEST {
            if let Some(opened) = egress::scope_of(option_id) {
                return self.allow_egress(&approval, option_id, opened, notes).await;
            }
        }
        if option_id != OPTION_ALLOW_ONCE {
            let (state, reason, note) = if option_id == OPTION_REQUEST_CHANGES {
                let Some(notes) = notes.or(reason) else {
                    return Err(SessionError::Unfit(vec![schema::FieldError {
                        field: "notes".into(),
                        message: "say what should change".into(),
                    }]));
                };
                (
                    CHANGES_REQUESTED,
                    "the operator asked for changes".to_string(),
                    Some(notes),
                )
            } else {
                (
                    REJECTED,
                    reason.unwrap_or_else(|| "the operator did not allow this call".into()),
                    notes,
                )
            };
            let decision = Decision {
                state,
                option_id,
                reason: Some(&reason),
                note: note.as_deref(),
                ..Default::default()
            };
            if !access.store.decide_approval(id, &decision)? {
                return Err(no_longer_waiting());
            }
            let decided = access.store.get_approval(id)?.unwrap_or(approval);
            access.manager.approval_decided(&decided, state).await;
            return Ok(());
        }
        let original: Value = serde_json::from_str(&approval.arguments)
            .map_err(|e| SessionError::Rejected(e.to_string()))?;
        if let Some(edited) = &answer.arguments {
            let errors = schema::check_edit(&approval.tool, &original, edited);
            if !errors.is_empty() {
                return Err(SessionError::Unfit(errors));
            }
        }
        // An approval an attached external session asked for before calls
        // went session-less runs as the channel's external caller now.
        let session = approval.session_id.clone().filter(|id| {
            !access
                .store
                .get_session(id)
                .ok()
                .flatten()
                .is_some_and(|s| s.harness_id == crate::session::external::HARNESS_ID)
        });
        let caller = match session {
            Some(session_id) => Caller::Session(session_id),
            None => Caller::External {
                lane: approval.lane.clone(),
            },
        };
        let ctx = CallContext {
            caller,
            channel: approval.channel.clone(),
            node_id: approval.node_id.clone(),
        };
        // What runs is the operator's rewrite, and a refusal is final whoever
        // wrote the words.
        let edited = answer.arguments.as_ref().map(Value::to_string);
        let run_args = answer.arguments.unwrap_or(original);
        let decision = Decision {
            state: RUNNING,
            option_id,
            edited_arguments: edited.as_deref(),
            note: notes.as_deref(),
            ..Default::default()
        };
        if !access.store.decide_approval(id, &decision)? {
            return Err(no_longer_waiting());
        }
        access.manager.approval_decided(&approval, RUNNING).await;
        let tools = Arc::clone(self);
        let name = approval.tool.clone();
        let id = id.to_string();
        tokio::spawn(async move {
            let outcome = tools.run_approved(&ctx, &name, &run_args).await;
            tools.settle_approval(&id, outcome).await;
        });
        Ok(())
    }

    /// Allow an egress ask: open its host to the asking session's own grant,
    /// for as long as the operator said, and record what was opened. Saving
    /// it to the repository is checked before anything is decided, so an
    /// entry that cannot take it leaves the card waiting with the reason.
    async fn allow_egress(
        &self,
        approval: &crate::store::approvals::ApprovalRow,
        option_id: &str,
        opened: egress::Opened,
        notes: Option<String>,
    ) -> Result<(), crate::session::SessionError> {
        use crate::session::SessionError;
        use crate::store::approvals::{Decision, FAILED, RUNNING, SUCCEEDED};
        let access = self
            .session
            .get()
            .ok_or_else(|| SessionError::Rejected("approvals are not available".into()))?;
        let host = egress::host_of(approval).map_err(SessionError::Rejected)?;
        let session_id = approval
            .session_id
            .clone()
            .ok_or_else(|| SessionError::Rejected("this ask names no session".into()))?;
        let saved = if opened.scope == "repository" {
            Some(
                crate::environment::save_session_host(&self.cfg, &access.store, &session_id, &host)
                    .map_err(SessionError::Rejected)?,
            )
        } else {
            None
        };
        let decision = Decision {
            state: RUNNING,
            option_id,
            note: notes.as_deref(),
            ..Default::default()
        };
        if !access.store.decide_approval(&approval.id, &decision)? {
            return Err(SessionError::Rejected(
                "this approval is no longer waiting".into(),
            ));
        }
        access.manager.approval_decided(approval, RUNNING).await;
        let widened = access
            .manager
            .egress_grants()
            .is_some_and(|grants| grants.widen_session(&session_id, &host, opened.until));
        let (state, result, reason) = if widened {
            let result = json!({
                "host": host,
                "scope": opened.scope,
                "for_secs": opened.until.map(|_| egress::ONCE.as_secs()),
                "repository": saved,
            });
            (SUCCEEDED, Some(result.to_string()), None)
        } else {
            (
                FAILED,
                None,
                Some(match &saved {
                    Some(repo) => format!(
                        "the session holds no grant any more, so nothing was opened now; \
                         {host} was saved to {repo}'s egress for its next session"
                    ),
                    None => "the session holds no grant any more; nothing was opened".into(),
                }),
            )
        };
        access
            .store
            .finish_approval(&approval.id, state, result.as_deref(), reason.as_deref())?;
        if let Some(settled) = access.store.get_approval(&approval.id)? {
            access.manager.approval_decided(&settled, state).await;
        }
        Ok(())
    }

    async fn run_approved(
        &self,
        ctx: &CallContext,
        name: &str,
        args: &Value,
    ) -> Result<Value, String> {
        let decision = self.decide(ctx, name, args);
        if decision.verdict == Verdict::Deny {
            return Err(refusal(decision));
        }
        let access = self.session.get().ok_or("approvals are not available")?;
        access
            .manager
            .approval_runnable(ctx)
            .map_err(|e| e.to_string())?;
        self.dispatch(ctx, name, args, true).await
    }

    async fn settle_approval(&self, id: &str, outcome: Result<Value, String>) {
        use crate::store::approvals::{FAILED, SUCCEEDED, UNCERTAIN};
        let Some(access) = self.session.get() else {
            return;
        };
        let (state, result, reason) = match &outcome {
            Ok(value) => (SUCCEEDED, Some(value.to_string()), None),
            Err(error) if remote_outcome_unknown(error) => (UNCERTAIN, None, Some(error.clone())),
            Err(error) => (FAILED, None, Some(error.clone())),
        };
        if let Err(e) =
            access
                .store
                .finish_approval(id, state, result.as_deref(), reason.as_deref())
        {
            tracing::error!(error = %e, approval = id, "failed to record an approval's outcome");
        }
        if let Ok(Some(approval)) = access.store.get_approval(id) {
            access.manager.approval_decided(&approval, state).await;
        }
    }

    fn decide(&self, ctx: &CallContext, name: &str, args: &Value) -> Decision {
        let live = self.cfg.live();
        let tool = self.policy.read().decide(&Request {
            channel: &ctx.channel,
            action: name,
            kind: Some(TOOL_KIND),
            resource: None,
            command: None,
            arguments: Some(args),
            doc_kinds: Some(&live.docs.kind),
        });
        if tool.verdict == Verdict::Deny {
            return tool;
        }
        if let Some((action, target, revision)) = consequential(name, args) {
            let Some(access) = self.session.get() else {
                return Decision {
                    verdict: Verdict::Ask,
                    rule_id: None,
                    reason: None,
                };
            };
            return crate::authority::decide(
                access.store.as_ref(),
                &self.policy.read(),
                &crate::authority::AuthorityQuery {
                    channel: &ctx.channel,
                    session_id: ctx.session_id(),
                    action,
                    target: &target,
                    revision: revision.as_deref(),
                    args,
                },
            )
            .unwrap_or(Decision {
                verdict: Verdict::Ask,
                rule_id: None,
                reason: None,
            });
        }
        tool
    }

    fn revalidate_tool_policy(
        &self,
        ctx: &CallContext,
        name: &str,
        args: &Value,
    ) -> Result<(), String> {
        let live = self.cfg.live();
        let decision = self.policy.read().decide(&Request {
            channel: &ctx.channel,
            action: name,
            kind: Some(TOOL_KIND),
            resource: None,
            command: None,
            arguments: Some(args),
            doc_kinds: Some(&live.docs.kind),
        });
        if decision.verdict == Verdict::Deny {
            return Err(refusal(decision));
        }
        Ok(())
    }

    /// Grants are re-read after every operator wait and immediately before a
    /// consequential request. Revocation and expiry never rely on an earlier
    /// cached decision.
    fn revalidate_consequential(
        &self,
        ctx: &CallContext,
        name: &str,
        args: &Value,
        one_shot: bool,
    ) -> Result<(), String> {
        self.revalidate_tool_policy(ctx, name, args)?;
        let Some((action, target, revision)) = consequential(name, args) else {
            return Ok(());
        };
        let decision = crate::authority::decide(
            self.session
                .get()
                .ok_or("authority requires a session")?
                .store
                .as_ref(),
            &self.policy.read(),
            &crate::authority::AuthorityQuery {
                channel: &ctx.channel,
                session_id: ctx.session_id(),
                action,
                target: &target,
                revision: revision.as_deref(),
                args,
            },
        )?;
        match decision.verdict {
            Verdict::Allow => Ok(()),
            Verdict::Deny => Err(refusal(decision)),
            Verdict::Ask if one_shot => Ok(()),
            Verdict::Ask => Err(format!(
                "{action} for {target} needs a current scoped authority grant"
            )),
        }
    }

    fn begin_consequential(
        &self,
        ctx: &CallContext,
        name: &str,
        args: &Value,
        one_shot: bool,
    ) -> Result<Option<ActionRecord>, String> {
        self.revalidate_tool_policy(ctx, name, args)?;
        let Some((action, target, revision)) = consequential(name, args) else {
            return Ok(None);
        };
        let access = self.session.get().ok_or("authority requires a session")?;
        let decision = crate::authority::decide(
            access.store.as_ref(),
            &self.policy.read(),
            &crate::authority::AuthorityQuery {
                channel: &ctx.channel,
                session_id: ctx.session_id(),
                action,
                target: &target,
                revision: revision.as_deref(),
                args,
            },
        )?;
        if decision.verdict != Verdict::Allow && !(one_shot && decision.verdict == Verdict::Ask) {
            return Err("authority changed before dispatch".into());
        }
        let evidence = serde_json::json!({
            "arguments": canonical_consequential_payload(name, args).expect("validated consequential arguments"),
        }).to_string();
        let operation_id = args["operation_id"]
            .as_str()
            .expect("validated operation id")
            .trim();
        let request_hash = crate::corpus::hash_body(&evidence);
        if let Some((state, outcome, recorded_hash)) = access
            .store
            .authority_action_existing(
                action,
                &target,
                &ctx.channel,
                revision.as_deref(),
                operation_id,
            )
            .map_err(|e| e.to_string())?
        {
            if recorded_hash != request_hash {
                return Err("operation_id was already used for a different request".into());
            }
            if state == "succeeded" {
                let outcome = outcome.unwrap_or_default();
                return Ok(Some(ActionRecord::Replay(
                    serde_json::from_str(&outcome).unwrap_or(serde_json::json!(outcome)),
                )));
            }
            return Err(format!(
                "this operation is {state}; reconcile its recorded external outcome before retrying"
            ));
        }
        let id = uuid::Uuid::now_v7().to_string();
        if let Err(error) = access
            .store
            .authority_action_begin(&crate::store::ActionBegin {
                id: &id,
                grant_id: decision.rule_id.as_deref(),
                action,
                target: &target,
                channel: &ctx.channel,
                session_id: ctx.session_id(),
                revision: revision.as_deref(),
                operation_id: Some(operation_id),
                evidence: &evidence,
            })
        {
            if let Some((state, outcome, recorded_hash)) = access
                .store
                .authority_action_existing(
                    action,
                    &target,
                    &ctx.channel,
                    revision.as_deref(),
                    operation_id,
                )
                .map_err(|e| e.to_string())?
            {
                if recorded_hash != request_hash {
                    return Err("operation_id was already used for a different request".into());
                }
                if state == "succeeded" {
                    let outcome = outcome.unwrap_or_default();
                    return Ok(Some(ActionRecord::Replay(
                        serde_json::from_str(&outcome).unwrap_or(serde_json::json!(outcome)),
                    )));
                }
                return Err(format!(
                    "this operation is {state}; reconcile its recorded external outcome before retrying"
                ));
            }
            return Err(error.to_string());
        }
        Ok(Some(ActionRecord::New(id)))
    }

    /// Handle one MCP JSON-RPC message. Returns `None` for notifications.
    pub async fn handle(&self, ctx: &CallContext, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned()?;
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => match negotiate_protocol(&params) {
                Ok(version) => Ok(json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "tracon", "version": env!("CARGO_PKG_VERSION") },
                })),
                Err(e) => Err(e),
            },
            "tools/list" => Ok(json!({ "tools": self.list_offered(ctx) })),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                self.call(ctx, name, &args)
                    .await
                    .map(|v| tool_result(&v, false))
                    .or_else(|e| Ok(tool_result(&json!(e), true)))
            }
            "ping" => Ok(json!({})),
            other => Err(format!("unsupported method {other}")),
        };
        Some(match result {
            Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
            Err(e) => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": e } })
            }
        })
    }
}

/// The revision an `initialize` asks for, refused by name when it is not one
/// this server serves. A client that names none gets this server's own.
fn negotiate_protocol(params: &Value) -> Result<String, String> {
    let Some(asked) = params.get("protocolVersion").and_then(Value::as_str) else {
        return Ok(PROTOCOL_VERSION.to_string());
    };
    if SUPPORTED_PROTOCOL_VERSIONS.contains(&asked) {
        return Ok(asked.to_string());
    }
    Err(format!(
        "client announces MCP protocol {asked}; this node supports {}",
        SUPPORTED_PROTOCOL_VERSIONS.join(", ")
    ))
}

/// Consequential verbs carry their scope in arguments. This parser is strict:
/// malformed input cannot fall through to an unscoped authority decision.
fn consequential_name(name: &str) -> bool {
    matches!(
        name,
        github::PR_MERGE | gitlab::MR_MERGE | gitlab::DEPLOY | jira::ISSUE_TRANSITION
    )
}

fn consequential(name: &str, args: &Value) -> Option<(&'static str, String, Option<String>)> {
    let string = |key| {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    if matches!(
        name,
        github::PR_MERGE | gitlab::MR_MERGE | gitlab::DEPLOY | jira::ISSUE_TRANSITION
    ) && !string("operation_id").is_some_and(|id| {
        id.len() >= 8 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    }) {
        return None;
    }
    match name {
        github::PR_MERGE => Some((
            crate::authority::MERGE,
            format!(
                "github:{}:pr:{}",
                string("repo")?.to_ascii_lowercase(),
                args.get("number").and_then(Value::as_i64)?,
            ),
            string("head_sha").map(str::to_string),
        )),
        gitlab::MR_MERGE => Some((
            crate::authority::MERGE,
            format!(
                "gitlab:{}:mr:{}",
                string("project")?,
                args.get("iid").and_then(Value::as_i64)?
            ),
            string("head_sha").map(str::to_string),
        )),
        gitlab::DEPLOY => Some((
            crate::authority::DEPLOY,
            format!(
                "gitlab:{}:environment:{}:pipeline:{}:job:{}",
                string("project")?,
                string("environment")?,
                args.get("pipeline_id").and_then(Value::as_i64)?,
                args.get("job_id").and_then(Value::as_i64)?,
            ),
            string("source_sha").map(str::to_string),
        )),
        jira::ISSUE_TRANSITION => {
            let transition = string("transition_id")?;
            if !transition
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            {
                return None;
            }
            Some((
                crate::authority::TICKET_TRANSITION,
                format!(
                    "jira:issue:{}:transition:{transition}",
                    string("key")?.to_ascii_uppercase(),
                ),
                None,
            ))
        }
        _ => None,
    }
}

/// The forge a tool reaches, for the tools that take a repository.
fn forge_of_tool(name: &str) -> Option<crate::forge::Forge> {
    if github::TOOLS.contains(&name) {
        Some(crate::forge::Forge::Github)
    } else if gitlab::TOOLS.contains(&name) {
        Some(crate::forge::Forge::Gitlab)
    } else {
        None
    }
}

/// The arguments with the provider's defaults written in, so the permission
/// prompt, the authority match and the replay record all show what will be
/// sent rather than leaving it to the provider call. `repo` is the calling
/// session's own repository, already matched to the tool's forge, and is the
/// default for the tool's `repo` or `project`; written in here, before
/// anything hashes the arguments, a retried call that names it and one that
/// leaves it out are the same request. `None` when nothing was missing.
fn with_provider_defaults(
    name: &str,
    args: &Value,
    repo: Option<&crate::forge::ForgeRepo>,
) -> Option<Value> {
    let mut defaults = Vec::new();
    match name {
        github::PR_MERGE => defaults.push(("method", Value::from("squash"))),
        gitlab::MR_MERGE => defaults.push(("squash", Value::from(true))),
        _ => {}
    }
    if let Some(repo) = repo {
        defaults.push((repo.argument(), Value::from(repo.path.as_str())));
    }
    let map = args.as_object()?;
    // A blank repository names nothing, so it is left out as surely as an
    // absent one; a blank `method` keeps the meaning it always had.
    let absent = |key: &str| match map.get(key) {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => repo.is_some_and(|r| r.argument() == key) && s.trim().is_empty(),
        Some(_) => false,
    };
    let missing: Vec<_> = defaults
        .into_iter()
        .filter(|(key, _)| absent(key))
        .collect();
    if missing.is_empty() {
        return None;
    }
    let mut map = map.clone();
    for (key, default) in missing {
        map.insert(key.into(), default);
    }
    Some(Value::Object(map))
}

/// `submit_review`'s `provider` and `project`, from the calling session's
/// repository when it has one. Both missing: both filled. One given: the
/// other is filled only when the given one is the session's own; otherwise
/// the call is refused rather than publishing to a pair nobody named. Both
/// given: untouched, wherever they point.
fn submission_defaults(
    args: &Value,
    repo: Option<&crate::forge::ForgeRepo>,
) -> Result<Option<Value>, String> {
    let (Some(repo), Some(map)) = (repo, args.as_object()) else {
        return Ok(None);
    };
    let given = |key: &str| {
        map.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let provider = repo.forge.name();
    let mismatch = |named: String| {
        format!(
            "this session's repository is {provider} `{}`, and the submission names {named}; \
             leave out both provider and project to publish to the session's repository, or \
             give both to publish elsewhere",
            repo.path
        )
    };
    match (given("provider"), given("project")) {
        (Some(_), Some(_)) => return Ok(None),
        (Some(asked), None) if crate::forge::Forge::parse(asked) != Some(repo.forge) => {
            return Err(mismatch(format!("provider `{asked}`")))
        }
        (None, Some(asked)) if !asked.eq_ignore_ascii_case(&repo.path) => {
            return Err(mismatch(format!("project `{asked}`")))
        }
        _ => {}
    }
    let mut filled = map.clone();
    if given("provider").is_none() {
        filled.insert("provider".into(), Value::from(provider));
    }
    if given("project").is_none() {
        filled.insert("project".into(), Value::from(repo.path.as_str()));
    }
    Ok(Some(Value::Object(filled)))
}

/// Normalize provider defaults and ignore arguments a consequential provider
/// does not consume before comparing a stable operation id on replay.
fn canonical_consequential_payload(name: &str, args: &Value) -> Option<Value> {
    let string = |key| {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let operation_id = string("operation_id")?;
    match name {
        github::PR_MERGE => Some(serde_json::json!({
            "repo": string("repo")?.to_ascii_lowercase(), "number": args.get("number")?.as_i64()?,
            "head_sha": string("head_sha")?, "method": string("method").unwrap_or("squash"),
            "operation_id": operation_id,
        })),
        gitlab::MR_MERGE => Some(serde_json::json!({
            "project": string("project")?, "iid": args.get("iid")?.as_i64()?,
            "head_sha": string("head_sha")?, "squash": args.get("squash").and_then(Value::as_bool).unwrap_or(true),
            "operation_id": operation_id,
        })),
        gitlab::DEPLOY => Some(serde_json::json!({
            "project": string("project")?, "pipeline_id": args.get("pipeline_id")?.as_i64()?,
            "job_id": args.get("job_id")?.as_i64()?, "environment": string("environment")?,
            "source_sha": string("source_sha")?, "operation_id": operation_id,
        })),
        jira::ISSUE_TRANSITION => Some(serde_json::json!({
            "key": string("key")?.to_ascii_uppercase(), "transition_id": string("transition_id")?,
            "operation_id": operation_id,
        })),
        _ => None,
    }
}

/// What makes two asks the same request on a channel: the tool and its
/// canonical arguments. A consequential call is keyed on the payload its
/// replay record would compare; anything else on its arguments, whose object
/// keys serialize in order.
fn request_key(name: &str, args: &Value) -> String {
    let payload = canonical_consequential_payload(name, args).unwrap_or_else(|| args.clone());
    crate::corpus::hash_body(&format!("{name}\u{1f}{payload}"))
}

/// Only a failed mutation transport or server error has an unknown remote
/// outcome. Read-only preflight failures are safe to retry.
fn remote_outcome_unknown(error: &str) -> bool {
    error.starts_with("mutation-outcome-unknown:")
}
fn refusal(decision: Decision) -> String {
    format!(
        "refused by policy{}: {}",
        decision
            .rule_id
            .map(|r| format!(" ({r})"))
            .unwrap_or_default(),
        decision.reason.unwrap_or_default()
    )
}

/// `name` plus its arguments on one line, bounded, for the policy haystack
/// and the queue card. Secrets never appear here: arguments are the
/// harness's own words. A profile is spelled out as `profile=<name>` ahead of
/// the JSON, so a deny rule can name one. A call that carries prose reads as
/// its target and first line instead.
pub fn summarize(name: &str, args: &Value) -> String {
    let mut s = match (
        prose_title(name, args),
        args.get("profile").and_then(Value::as_str),
    ) {
        (Some(title), _) => title,
        (None, Some(p)) => format!("{name} profile={p} {args}"),
        (None, None) => format!("{name} {args}"),
    };
    bound(&mut s);
    s
}

/// What a held call would replace, as it stands now, for the approval page
/// to show the proposal against: fields merged into the approval's details.
/// `None` for a tool that replaces nothing the node holds.
pub fn approval_context(
    store: &crate::store::Store,
    channel: &str,
    tool: &str,
    args: &Value,
) -> Result<Option<Value>, crate::store::StoreError> {
    match tool {
        docs::DOC_WRITE => {
            let current = docs::replaced(store, channel, args)?;
            Ok(Some(docs::approval_context(current.as_ref(), args)))
        }
        memory::AMEND => Ok(Some(memory::approval_context(store, channel, args)?)),
        _ => Ok(None),
    }
}

/// A title cut to the length a card and a push notification carry.
fn bound(s: &mut String) {
    if s.len() > 400 {
        let mut cut = 400;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push('…');
    }
}

/// A Jira transition titled by the status it leads to, which the operator
/// knows, rather than by its id, which they do not. Jira is asked at request
/// time; when it cannot say, the title names the id.
fn transition_title(args: &Value, to: &str) -> String {
    let key = args
        .get("key")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .trim();
    let mut s = format!("{} {key}: move to {to}", jira::ISSUE_TRANSITION);
    bound(&mut s);
    s
}

/// A title a person reads for a call that carries prose: what it acts on and
/// the first line of what it says, rather than its arguments as JSON.
fn prose_title(name: &str, args: &Value) -> Option<String> {
    let text = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
    };
    let id = |key: &str| match args.get(key) {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        _ => "?".into(),
    };
    let first_line =
        |key: &str| text(key).and_then(|t| t.lines().map(str::trim).find(|l| !l.is_empty()));
    let with = |head: String, said: Option<&str>| match said {
        Some(said) => format!("{head}: {said}"),
        None => head,
    };
    let short = |key: &str| id(key).chars().take(7).collect::<String>();
    // Variable names, not values: which ones are set is what an operator
    // weighs, and the page shows the values.
    let with_variables = |head: String| {
        let names: Vec<&str> = args
            .get("variables")
            .and_then(Value::as_object)
            .map(|v| v.keys().map(String::as_str).collect())
            .unwrap_or_default();
        if names.is_empty() {
            head
        } else {
            format!("{head} with {}", names.join(", "))
        }
    };
    Some(match name {
        jira::ISSUE_CREATE => with(
            format!("{name} {} {}", id("project"), id("type")),
            text("summary"),
        ),
        jira::ISSUE_UPDATE => {
            let fields: Vec<&str> = ["summary", "description", "priority", "labels", "parent"]
                .into_iter()
                .filter(|f| args.get(*f).is_some())
                .collect();
            let head = format!("{name} {} ({})", id("key"), fields.join(", "));
            with(head, text("summary"))
        }
        jira::ISSUE_COMMENT => with(format!("{name} {}", id("key")), first_line("body")),
        github::PR_COMMENT | github::ISSUE_COMMENT | github::PR_REPLY => with(
            format!("{name} {}#{}", id("repo"), id("number")),
            first_line("body"),
        ),
        gitlab::MR_COMMENT | gitlab::MR_REPLY => with(
            format!("{name} {}!{}", id("project"), id("iid")),
            first_line("body"),
        ),
        docs::DOC_WRITE | docs::DOC_DELETE => format!("{name} {}", id("slug")),
        memory::RETAIN => with(format!("{name} {}", id("kind")), first_line("body")),
        memory::AMEND => with(
            format!("{name} {}", id("id").chars().take(8).collect::<String>()),
            first_line("reason"),
        ),
        work::WORK_UPDATE => {
            let fields: Vec<String> = [
                "title", "body", "priority", "deps", "project", "worktree", "state",
            ]
            .into_iter()
            .filter(|f| args.get(*f).is_some_and(|v| !v.is_null()))
            .map(|f| match (f, text(f)) {
                ("state" | "priority", Some(v)) => format!("{f} {v}"),
                _ => f.to_string(),
            })
            .collect();
            let head = format!(
                "{name} {} ({})",
                id("id").chars().take(8).collect::<String>(),
                fields.join(", ")
            );
            with(head, first_line("title"))
        }
        work::WORK_DELETE | work::WORK_SHOW => match text("id") {
            Some(item) => format!("{name} {}", item.chars().take(8).collect::<String>()),
            None => name.to_string(),
        },
        work::WORK_LIST => {
            let narrowed: Vec<String> = ["project", "worktree", "state", "query"]
                .into_iter()
                .filter_map(|f| text(f).map(|v| format!("{f} {v}")))
                .collect();
            if narrowed.is_empty() {
                name.to_string()
            } else {
                format!("{name} ({})", narrowed.join(", "))
            }
        }
        work::BRIEF_NOTE => with(format!("{name} {}", id("field")), first_line("text")),
        work::CRITERIA_LINK => with(
            format!("{name} {} {}", id("criterion"), id("kind")),
            first_line("value"),
        ),
        review::SUBMIT_REPORT => with(name.to_string(), text("title")),
        jira::ISSUE_TRANSITION => {
            format!("{name} {}: transition {}", id("key"), id("transition_id"))
        }
        github::PR_MERGE => format!(
            "{name} {}#{}: {} at {}",
            id("repo"),
            id("number"),
            text("method").unwrap_or("squash"),
            short("head_sha")
        ),
        gitlab::MR_MERGE => format!(
            "{name} {}!{}: {} at {}",
            id("project"),
            id("iid"),
            if args.get("squash") == Some(&Value::Bool(false)) {
                "merge"
            } else {
                "squash"
            },
            short("head_sha")
        ),
        github::PR_READY => format!(
            "{name} {}#{}: {}",
            id("repo"),
            id("number"),
            ready_or_draft(args)
        ),
        gitlab::MR_READY => format!(
            "{name} {}!{}: {}",
            id("project"),
            id("iid"),
            ready_or_draft(args)
        ),
        gitlab::JOB_PLAY => {
            with_variables(format!("{name} {}: job {}", id("project"), id("job_id")))
        }
        gitlab::PIPELINE_RUN => with_variables(format!("{name} {}: {}", id("project"), id("ref"))),
        gitlab::PIPELINE_LIST_BY_SHA => format!("{name} {}: {}", id("project"), short("sha")),
        gitlab::DEPLOY => format!(
            "{name} {}: {} from job {} of pipeline {} at {}",
            id("project"),
            id("environment"),
            id("job_id"),
            id("pipeline_id"),
            short("source_sha")
        ),
        github::RUN_RERUN => format!("{name} {}: failed jobs of run {}", id("repo"), id("run_id")),
        gitlab::PIPELINE_RETRY => format!(
            "{name} {}: failed jobs of pipeline {}",
            id("project"),
            id("pipeline_id")
        ),
        github::RUN_DISPATCH => {
            let inputs: Vec<&str> = args
                .get("inputs")
                .and_then(Value::as_object)
                .map(|v| v.keys().map(String::as_str).collect())
                .unwrap_or_default();
            let head = format!("{name} {}: {} on {}", id("repo"), id("workflow"), id("ref"));
            if inputs.is_empty() {
                head
            } else {
                format!("{head} with {}", inputs.join(", "))
            }
        }
        services::START => format!("{name} {}", id("name")),
        setup::PROPOSE => with(format!("{name} {}", id("repo")), first_line("why")),
        setup::TRY => format!(
            "{name} {}: build {} from {}",
            id("repo"),
            id("dockerfile"),
            id("build_source")
        ),
        _ => return None,
    })
}

/// What a `pr_ready` or `mr_ready` call asks for, in words: ready unless it
/// says `ready: false`.
fn ready_or_draft(args: &Value) -> &'static str {
    if args.get("ready") == Some(&Value::Bool(false)) {
        "back to draft"
    } else {
        "ready for review"
    }
}

fn tool_result(value: &Value, is_error: bool) -> Value {
    let text = match value {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    };
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prose_call_is_titled_by_its_target_and_first_line() {
        let create = json!({ "project": "WRK", "type": "Task", "summary": "Rotate keys", "description": "h1. Why" });
        assert_eq!(
            summarize(jira::ISSUE_CREATE, &create),
            "issue_create WRK Task: Rotate keys"
        );
        let update = json!({ "key": "WRK-2", "description": "d", "labels": ["a"] });
        assert_eq!(
            summarize(jira::ISSUE_UPDATE, &update),
            "issue_update WRK-2 (description, labels)"
        );
        let comment = json!({ "repo": "o/n", "number": 4, "body": "\n  Looks good.\nMore." });
        assert_eq!(
            summarize(github::PR_COMMENT, &comment),
            "pr_comment o/n#4: Looks good."
        );
        assert_eq!(
            summarize(github::ISSUE_COMMENT, &comment),
            "github_issue_comment o/n#4: Looks good."
        );
        let note = json!({ "project": "g/p", "iid": 9, "body": "Thanks" });
        assert_eq!(
            summarize(gitlab::MR_COMMENT, &note),
            "mr_comment g/p!9: Thanks"
        );
        let fact = json!({ "kind": "fact", "body": "The importer retries twice.\nThen gives up." });
        assert_eq!(
            summarize(memory::RETAIN, &fact),
            "retain fact: The importer retries twice."
        );
        let doc = json!({ "slug": "plan-foo", "body": "# Plan" });
        assert_eq!(summarize(docs::DOC_WRITE, &doc), "doc_write plan-foo");
        let id = "0b0e29c44da42ee5fef94df7b1b9eff7";
        assert_eq!(
            summarize(
                work::WORK_UPDATE,
                &json!({ "id": id, "priority": "Urgent", "body": "x", "title": "Lists past 100" })
            ),
            "work_update 0b0e29c4 (title, body, priority Urgent): Lists past 100"
        );
        assert_eq!(
            summarize(work::WORK_DELETE, &json!({ "id": id })),
            "work_delete 0b0e29c4"
        );
        assert_eq!(
            summarize(
                work::WORK_LIST,
                &json!({ "project": "tracon", "state": "all" })
            ),
            "work_list (project tracon, state all)"
        );
        assert_eq!(summarize(work::WORK_SHOW, &json!({})), "work_show");
        let query = json!({ "profile": "qa", "sql": "select 1" });
        assert!(summarize("query", &query).starts_with("query profile=qa {"));
    }

    /// A gated call with no prose is titled in words too, on the card and in
    /// the push, rather than as its arguments.
    #[test]
    fn a_gated_call_is_titled_by_what_it_does() {
        let merge = json!({ "repo": "o/n", "number": 412, "head_sha": "3e1f0a2c9b8d", "method": "squash", "operation_id": "op" });
        assert_eq!(
            summarize(github::PR_MERGE, &merge),
            "pr_merge o/n#412: squash at 3e1f0a2"
        );
        let mr = json!({ "project": "g/p", "iid": 9, "head_sha": "abcdef12", "squash": false, "operation_id": "op" });
        assert_eq!(
            summarize(gitlab::MR_MERGE, &mr),
            "mr_merge g/p!9: merge at abcdef1"
        );
        let ready = json!({ "repo": "o/n", "number": 595 });
        assert_eq!(
            summarize(github::PR_READY, &ready),
            "pr_ready o/n#595: ready for review"
        );
        let draft = json!({ "project": "g/p", "iid": 9, "ready": false });
        assert_eq!(
            summarize(gitlab::MR_READY, &draft),
            "mr_ready g/p!9: back to draft"
        );
        let rerun = json!({ "repo": "o/n", "run_id": 11893472205_u64 });
        assert_eq!(
            summarize(github::RUN_RERUN, &rerun),
            "run_rerun o/n: failed jobs of run 11893472205"
        );
        let retry = json!({ "project": "g/p", "pipeline_id": 12 });
        assert_eq!(
            summarize(gitlab::PIPELINE_RETRY, &retry),
            "pipeline_retry g/p: failed jobs of pipeline 12"
        );
        let dispatch = json!({ "repo": "o/n", "workflow": "renovate.yaml", "ref": "main",
                               "inputs": { "LOG_LEVEL": "debug" } });
        assert_eq!(
            summarize(github::RUN_DISPATCH, &dispatch),
            "run_dispatch o/n: renovate.yaml on main with LOG_LEVEL"
        );
        let service = json!({ "name": "postgres", "wait_secs": 30 });
        assert_eq!(
            summarize(services::START, &service),
            "service_start postgres"
        );
        let propose = json!({ "repo": "/src/orbit", "checks": ["cargo test"], "why": "\nTried it twice.\nBoth passed." });
        assert_eq!(
            summarize(setup::PROPOSE, &propose),
            "repo_setup_propose /src/orbit: Tried it twice."
        );
        let play = json!({ "project": "g/p", "job_id": 5, "variables": { "TARGET": "qa" } });
        assert_eq!(
            summarize(gitlab::JOB_PLAY, &play),
            "job_play g/p: job 5 with TARGET"
        );
        let run = json!({ "project": "g/p", "ref": "main" });
        assert_eq!(
            summarize(gitlab::PIPELINE_RUN, &run),
            "pipeline_run g/p: main"
        );
        let deploy = json!({ "project": "g/p", "pipeline_id": 12, "job_id": 7, "environment": "qa", "source_sha": "abcdef12", "operation_id": "op" });
        assert_eq!(
            summarize(gitlab::DEPLOY, &deploy),
            "deploy g/p: qa from job 7 of pipeline 12 at abcdef1"
        );
        // Without Jira's word on where it leads, a transition names its id.
        let transition = json!({ "key": "WRK-1", "transition_id": "31", "operation_id": "op" });
        assert_eq!(
            summarize(jira::ISSUE_TRANSITION, &transition),
            "issue_transition WRK-1: transition 31"
        );
        assert_eq!(
            transition_title(&transition, "In Review"),
            "issue_transition WRK-1: move to In Review"
        );
    }

    /// Every tool the operator can be asked about has a title in words, so a
    /// new gated tool cannot reach the card or a push as raw JSON unnoticed.
    /// Asked means: the shipped bundle asks with no arguments, or the node
    /// always holds it.
    #[test]
    fn every_asked_tool_is_titled_in_words() {
        let policy = crate::policy::Policy::shipped();
        let none = json!({});
        let mut asked: Vec<&str> = schema::tool_names()
            .filter(|name| {
                let decision = policy.decide(&Request {
                    channel: "work",
                    action: name,
                    kind: Some(TOOL_KIND),
                    resource: None,
                    command: None,
                    arguments: Some(&none),
                    doc_kinds: None,
                });
                decision.verdict == Verdict::Ask
            })
            // Routed before the policy: these hold their own approval, with
            // their own title (egress), or none at all.
            .filter(|name| {
                ![
                    operator::ASK,
                    operator::NOTIFY,
                    operator::REPORT,
                    operator::REPORT_STATUS,
                    operator::QUESTION_STATUS,
                    egress::REQUEST,
                    setup::DRAFT,
                    setup::TRY,
                ]
                .contains(name)
            })
            .chain([setup::PROPOSE])
            .collect();
        asked.sort();
        asked.dedup();
        assert!(asked.contains(&github::PR_MERGE), "{asked:?}");
        let raw: Vec<String> = asked
            .iter()
            .map(|name| summarize(name, &none))
            .filter(|title| title.contains('{'))
            .collect();
        assert!(raw.is_empty(), "titled as JSON: {raw:?}");
    }

    #[test]
    fn a_merge_default_is_written_into_the_arguments_that_are_approved() {
        let pr = json!({ "repo": "o/n", "number": 1, "head_sha": "abcdef1", "operation_id": "op" });
        let resolved = with_provider_defaults(github::PR_MERGE, &pr, None).unwrap();
        assert_eq!(resolved["method"], "squash");
        assert_eq!(
            canonical_consequential_payload(github::PR_MERGE, &resolved),
            canonical_consequential_payload(github::PR_MERGE, &pr),
        );
        let chosen = json!({ "method": "rebase", "repo": "o/n" });
        assert!(with_provider_defaults(github::PR_MERGE, &chosen, None).is_none());

        let mr = json!({ "project": "g/p", "iid": 2, "head_sha": "abc", "operation_id": "op" });
        assert_eq!(
            with_provider_defaults(gitlab::MR_MERGE, &mr, None).unwrap()["squash"],
            true
        );
        let kept = json!({ "squash": false });
        assert!(with_provider_defaults(gitlab::MR_MERGE, &kept, None).is_none());
        assert!(with_provider_defaults(github::PR_COMMENT, &pr, None).is_none());
    }

    /// The session's repository is written in before anything hashes the
    /// arguments: a merge retried without `repo` is the same request, under
    /// the same replay record, as the one that named it.
    #[test]
    fn the_session_repository_is_a_default_like_any_other() {
        let github = crate::forge::ForgeRepo::from_remote("github.com/o/n").unwrap();
        let gitlab = crate::forge::ForgeRepo::from_remote("gitlab.com/g/p").unwrap();

        let named =
            json!({ "repo": "o/n", "number": 1, "head_sha": "abcdef1", "operation_id": "op" });
        let unnamed = json!({ "number": 1, "head_sha": "abcdef1", "operation_id": "op" });
        let filled = with_provider_defaults(github::PR_MERGE, &unnamed, Some(&github)).unwrap();
        assert_eq!(filled["repo"], "o/n");
        assert_eq!(
            canonical_consequential_payload(github::PR_MERGE, &filled),
            canonical_consequential_payload(
                github::PR_MERGE,
                &with_provider_defaults(github::PR_MERGE, &named, Some(&github)).unwrap()
            ),
        );
        assert_eq!(
            request_key(github::PR_MERGE, &filled),
            request_key(
                github::PR_MERGE,
                &with_provider_defaults(github::PR_MERGE, &named, Some(&github)).unwrap()
            ),
        );
        // Without the default there is no repository to hash at all.
        assert!(canonical_consequential_payload(github::PR_MERGE, &unnamed).is_none());

        // An explicit repository wins; a blank one names nothing.
        let other = json!({ "repo": "else/where", "branch": "main" });
        assert!(with_provider_defaults(github::RUN_STATUS, &other, Some(&github)).is_none());
        let blank = json!({ "repo": " ", "branch": "main" });
        assert_eq!(
            with_provider_defaults(github::RUN_STATUS, &blank, Some(&github)).unwrap()["repo"],
            "o/n"
        );

        // GitLab's tools take it as `project`.
        let mr = json!({ "iid": 2, "head_sha": "abc", "operation_id": "op" });
        let filled = with_provider_defaults(gitlab::MR_MERGE, &mr, Some(&gitlab)).unwrap();
        assert_eq!(
            (&filled["project"], &filled["squash"]),
            (&json!("g/p"), &json!(true))
        );
        assert!(filled.get("repo").is_none());

        // Every forge tool takes the repository by the forge's own name for it.
        for (tools, names, key) in [
            (github::definitions(), github::TOOLS, "repo"),
            (gitlab::definitions(), gitlab::TOOLS, "project"),
        ] {
            assert_eq!(tools.len(), names.len());
            for tool in tools {
                let name = tool["name"].as_str().unwrap();
                assert!(forge_of_tool(name).is_some(), "{name} is not a forge tool");
                assert!(
                    tool["inputSchema"]["properties"].get(key).is_some(),
                    "{name} has no {key}"
                );
            }
        }
        assert_eq!(forge_of_tool(review::SUBMIT), None);
    }

    /// `submit_review` takes the session's repository as provider and
    /// project: both filled when both are missing, the other filled when the
    /// one given is the session's own, refused when it is not, and never
    /// touched when both are given.
    #[test]
    fn a_submission_defaults_to_the_session_repository_or_is_refused() {
        let github = crate::forge::ForgeRepo::from_remote("github.com/o/n").unwrap();
        let gitlab = crate::forge::ForgeRepo::from_remote("gitlab.com/g/sub/p").unwrap();
        let bare = json!({ "title": "t", "body": "b" });

        let filled = submission_defaults(&bare, Some(&github)).unwrap().unwrap();
        assert_eq!(
            (&filled["provider"], &filled["project"]),
            (&json!("github"), &json!("o/n"))
        );
        let named = json!({ "title": "t", "body": "b", "provider": "github", "project": "o/n" });
        assert_eq!(
            request_key(review::SUBMIT, &filled),
            request_key(review::SUBMIT, &named)
        );
        let filled = submission_defaults(&bare, Some(&gitlab)).unwrap().unwrap();
        assert_eq!(
            (&filled["provider"], &filled["project"]),
            (&json!("gitlab"), &json!("g/sub/p"))
        );

        // Both given: untouched, wherever they point.
        let elsewhere = json!({ "provider": "gitlab", "project": "x/y" });
        assert_eq!(submission_defaults(&elsewhere, Some(&github)), Ok(None));
        // One given and consistent: the other is filled. Blank is missing.
        for (given, filled_key, value) in [
            (json!({ "provider": "github" }), "project", "o/n"),
            (
                json!({ "project": "O/N", "provider": " " }),
                "provider",
                "github",
            ),
        ] {
            let out = submission_defaults(&given, Some(&github)).unwrap().unwrap();
            assert_eq!(out[filled_key], value, "{given}");
        }
        // One given and not the session's: refused, naming both sides.
        for given in [json!({ "provider": "gitlab" }), json!({ "project": "x/y" })] {
            let refused = submission_defaults(&given, Some(&github)).unwrap_err();
            assert!(refused.contains("github `o/n`"), "{refused}");
            assert!(refused.contains("give both"), "{refused}");
        }
        // No session repository: nothing filled, nothing refused here.
        assert_eq!(submission_defaults(&bare, None), Ok(None));
        assert_eq!(
            submission_defaults(&json!({ "project": "x/y" }), None),
            Ok(None)
        );
    }

    fn tools(store: &str) -> Tools {
        Tools {
            broker: toml::from_str::<crate::broker::Broker>(store)
                .unwrap()
                .shared(),
            cfg: Arc::new(Config::default()),
            policy: Policy::shipped_shared(),
            http: reqwest::Client::new(),
            session: Default::default(),
        }
    }

    fn ctx(channel: &str) -> CallContext {
        CallContext::session("s", channel, "n1")
    }

    const STORE: &str = r#"
        [credentials.consulta]
        channels = ["work"]
        [credentials.consulta.env]
        DB_BACKEND = "sqlite"
    "#;

    #[tokio::test]
    async fn tools_are_offered_only_to_a_bound_channel() {
        let t = tools(STORE);
        assert_eq!(t.list("work", "n1").len(), 2);
        assert!(t.list("personal", "n1").is_empty());
    }

    const PROFILES: &str = r#"
        [credentials.consulta-qa]
        channels = ["work"]
        [credentials.consulta-qa.env]
        DB_BACKEND = "sqlite"
        [credentials.consulta-prd]
        channels = ["work"]
        [credentials.consulta-prd.env]
        DB_BACKEND = "sqlite"
    "#;

    #[tokio::test]
    async fn a_channels_profiles_are_offered_from_the_credentials_it_holds() {
        let t = tools(PROFILES);
        let list = t.list("work", "n1");
        assert_eq!(list.len(), 2);
        let schema = &list[0]["inputSchema"];
        assert_eq!(
            schema["properties"]["profile"]["enum"],
            json!(["prd", "qa"])
        );
        // Two and no default: the call has to say which.
        assert!(schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("profile")));
        assert!(t.list("personal", "n1").is_empty());
        // Only the default: nothing to choose, so nothing is asked.
        let plain = tools(STORE).list("work", "n1");
        assert!(plain[0]["inputSchema"]["properties"]["profile"].is_null());
    }

    #[test]
    fn a_profile_resolves_to_its_credential() {
        let both = vec!["default".to_string(), "qa".into()];
        assert_eq!(consulta::credential_for(None, &both).unwrap(), "consulta");
        assert_eq!(
            consulta::credential_for(Some("qa"), &both).unwrap(),
            "consulta-qa"
        );
        let err = consulta::credential_for(Some("prd"), &both).unwrap_err();
        assert!(err.contains("qa"), "{err}");
        let two = vec!["prd".to_string(), "qa".into()];
        assert!(consulta::credential_for(None, &two).is_err());
        let one = vec!["qa".to_string()];
        assert_eq!(consulta::credential_for(None, &one).unwrap(), "consulta-qa");
        assert_eq!(
            consulta::profiles(&["consulta", "consulta-tst", "gh", "consulta-"]),
            vec!["default".to_string(), "tst".into()]
        );
    }

    #[tokio::test]
    async fn a_deny_rule_can_name_a_profile() {
        let mut t = tools(PROFILES);
        t.policy = Arc::new(parking_lot::RwLock::new(
            toml::from_str(
                r#"
            version = 9
            [[rule]]
            id = "no-production"
            verdict = "deny"
            reason = "Production is read by hand."
            kinds = ["tool"]
            matches = ["profile=prd"]
            "#,
            )
            .unwrap(),
        ));
        let res = t
            .handle(
                &ctx("work"),
                &json!({"jsonrpc":"2.0","id":7,"method":"tools/call",
                        "params":{"name":"query","arguments":{"sql":"SELECT 1","profile":"prd"}}}),
            )
            .await
            .unwrap();
        assert_eq!(res["result"]["isError"], true);
        let text = res["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("no-production"), "{text}");
    }

    #[tokio::test]
    async fn initialize_and_tools_list_answer() {
        let t = tools(STORE);
        let init = t
            .handle(
                &ctx("work"),
                &json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
            )
            .await
            .unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "tracon");
        let list = t
            .handle(
                &ctx("work"),
                &json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            )
            .await
            .unwrap();
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"query") && names.contains(&"describe"));
    }

    /// The only client here is a harness this node launched. One announcing
    /// an MCP revision the node does not serve is a compatibility fault worth
    /// seeing: it is refused by name rather than answered with a version
    /// number it did not ask for and cannot read.
    #[tokio::test]
    async fn an_unsupported_mcp_revision_is_refused_by_name() {
        let t = tools(STORE);
        let res = t
            .handle(
                &ctx("work"),
                &json!({"jsonrpc":"2.0","id":1,"method":"initialize",
                        "params":{"protocolVersion":"2099-01-01"}}),
            )
            .await
            .unwrap();
        assert!(res["result"].is_null(), "{res}");
        let message = res["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains("2099-01-01"), "{message}");
        assert!(message.contains(PROTOCOL_VERSION), "{message}");
    }

    /// A revision this server does serve is answered in that revision: the
    /// surface is identical across them, and telling a client a number it did
    /// not ask for is what makes it give up.
    #[tokio::test]
    async fn a_supported_mcp_revision_is_answered_in_its_own_terms() {
        let t = tools(STORE);
        for asked in SUPPORTED_PROTOCOL_VERSIONS {
            let res = t
                .handle(
                    &ctx("work"),
                    &json!({"jsonrpc":"2.0","id":1,"method":"initialize",
                            "params":{"protocolVersion":asked}}),
                )
                .await
                .unwrap();
            assert_eq!(res["result"]["protocolVersion"], asked, "{res}");
        }
    }

    #[tokio::test]
    async fn a_notification_gets_no_response() {
        let t = tools(STORE);
        assert!(t
            .handle(
                &ctx("work"),
                &json!({"jsonrpc":"2.0","method":"notifications/initialized"})
            )
            .await
            .is_none());
    }

    #[tokio::test]
    async fn an_unbound_channel_cannot_call_the_tool_even_knowing_its_name() {
        // Not offering the tool is presentation; refusing the call is the gate.
        let t = tools(STORE);
        let res = t
            .handle(
                &ctx("personal"),
                &json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
                        "params":{"name":"query","arguments":{"sql":"SELECT 1"}}}),
            )
            .await
            .unwrap();
        assert_eq!(res["result"]["isError"], true);
        let text = res["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("not bound"), "{text}");
    }

    #[tokio::test]
    async fn a_tool_the_policy_denies_is_refused_with_the_reason() {
        let mut t = tools(STORE);
        t.policy = Arc::new(parking_lot::RwLock::new(
            toml::from_str(
                r#"
            version = 9
            [[rule]]
            id = "no-warehouse"
            verdict = "deny"
            reason = "The warehouse is closed today."
            kinds = ["tool"]
            matches = ["query"]
            "#,
            )
            .unwrap(),
        ));
        let res = t
            .handle(
                &ctx("work"),
                &json!({"jsonrpc":"2.0","id":5,"method":"tools/call",
                        "params":{"name":"query","arguments":{"sql":"SELECT 1"}}}),
            )
            .await
            .unwrap();
        assert_eq!(res["result"]["isError"], true);
        let text = res["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("no-warehouse") && text.contains("closed today"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_tool_the_policy_does_not_cover_is_not_run_unattended() {
        // Empty policy: everything is asked, and with no session to ask
        // through the call fails rather than proceeds.
        let mut t = tools(STORE);
        t.policy = Default::default();
        let res = t
            .handle(
                &ctx("work"),
                &json!({"jsonrpc":"2.0","id":6,"method":"tools/call",
                        "params":{"name":"query","arguments":{"sql":"SELECT 1"}}}),
            )
            .await
            .unwrap();
        assert_eq!(res["result"]["isError"], true);
        let text = res["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("approval"), "{text}");
    }

    #[tokio::test]
    async fn the_guard_refuses_before_anything_is_spawned() {
        let t = tools(STORE);
        let res = t
            .handle(
                &ctx("work"),
                &json!({"jsonrpc":"2.0","id":4,"method":"tools/call",
                        "params":{"name":"query","arguments":{"sql":"DELETE FROM people"}}}),
            )
            .await
            .unwrap();
        assert_eq!(res["result"]["isError"], true);
        let text = res["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.to_lowercase().contains("delete"), "{text}");
    }
}
