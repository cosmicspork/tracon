//! The tools the node exposes to a harness, over MCP.
//!
//! The transport is the gateway's forward: the harness can reach the node and
//! nothing else, and it carries a token minted per session. With `[external]`
//! set, a harness the operator runs themselves reaches the same tools through
//! the operator door instead, on a session it attaches to a channel. Either
//! way a tool is the only shape a credential ever reaches the harness in — as
//! something it may ask the node to do, never as something it holds.

pub mod consulta;
pub mod docs;
pub mod github;
pub mod gitlab;
pub mod jira;
pub mod memory;
pub mod operator;
pub mod review;
pub mod work;

use std::sync::Arc;

use serde_json::{json, Value};

use crate::{
    acp::types::{PermissionOption, OPTION_ALLOW_ONCE, OPTION_REJECT_ONCE},
    adapter::{PermissionReply, PermissionRequest},
    broker::SharedBroker,
    config::Config,
    policy::{Decision, Policy, Request, Verdict},
};

/// The policy kind a brokered tool call is evaluated under. Rules with
/// `kinds = ["tool"]` match the tool's name exactly for allow, and any
/// substring of name-plus-arguments for deny.
pub const TOOL_KIND: &str = "tool";

/// The MCP protocol version the node speaks.
const PROTOCOL_VERSION: &str = "2025-06-18";

pub struct Tools {
    pub broker: SharedBroker,
    pub cfg: Arc<Config>,
    /// Every call is decided here before the broker is touched: the same
    /// bundle that answers the harness's own permission requests answers
    /// what the node will do on its behalf. A denied call returns the rule's
    /// reason; one the policy does not cover is put to the operator.
    pub policy: Arc<std::sync::RwLock<Policy>>,
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
    pub session_id: String,
    pub channel: String,
    pub node_id: String,
}

struct GatedCall {
    arguments: Option<Value>,
    one_shot: bool,
}

enum ActionRecord {
    New(String),
    Replay(Value),
}

impl Tools {
    /// The tools a review session gets: what it needs to read, and its
    /// verdict. Nothing that writes, publishes, or reaches a forge.
    pub const REVIEW_TOOLS: &'static [&'static str] = &[
        memory::RECALL,
        docs::DOC_READ,
        docs::DOC_SEARCH,
        review::VERDICT,
    ];

    /// What an attached external harness is not offered: a verdict is what a
    /// review session the node started gives on someone else's change.
    pub const NOT_EXTERNAL: &'static [&'static str] = &[review::VERDICT];

    /// Tool definitions for a channel, narrowed by phase: a review session
    /// sees only [`Self::REVIEW_TOOLS`].
    pub fn list_for(
        &self,
        channel: &str,
        node_id: &str,
        phase: crate::session::Phase,
    ) -> Vec<Value> {
        let all = self.list(channel, node_id);
        match phase {
            crate::session::Phase::Review => all
                .into_iter()
                .filter(|t| {
                    t["name"]
                        .as_str()
                        .is_some_and(|n| Self::REVIEW_TOOLS.contains(&n))
                })
                .collect(),
            _ => all,
        }
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
            out.extend(review::definitions());
            // Memory and documents need no credential either: the corpus is
            // the node's own, and reading it is what every session starts by
            // doing. This means every session gets an MCP server.
            out.extend(memory::definitions());
            out.extend(docs::definitions());
            // These are intervention-only: asking, pinging, and drafting a
            // report do not touch a credential or widen a tool policy.
            out.extend(operator::definitions());
            out.extend(work::definitions());
        }
        out
    }

    pub async fn call(&self, ctx: &CallContext, name: &str, args: &Value) -> Result<Value, String> {
        // A plan session's own plan document is the phase's artifact: writing
        // that one slug is what the session exists to do, so it is not asked.
        let plan_write = name == docs::DOC_WRITE && self.is_plan_artifact(ctx, args);
        if self.is_review_session(ctx) && !Self::REVIEW_TOOLS.contains(&name) {
            return Err(format!(
                "{name} is not offered to a review session; give a verdict with {}",
                review::VERDICT
            ));
        }
        if Self::NOT_EXTERNAL.contains(&name) && self.is_external_session(ctx) {
            return Err(format!(
                "{name} is for a review session the node starts; put your own change up with {}",
                review::SUBMIT
            ));
        }
        if matches!(name, operator::ASK | operator::NOTIFY | operator::REPORT) {
            let access = self
                .session
                .get()
                .ok_or("operator interventions are not available on this node")?;
            return operator::call(&access.store, &access.manager, &self.cfg, ctx, name, args)
                .await;
        }
        if consequential_name(name) && consequential(name, args).is_none() {
            return Err("consequential calls require a valid operation_id and canonical arguments".into());
        }
        let gated = if plan_write {
            GatedCall { arguments: None, one_shot: false }
        } else {
            self.gate(ctx, name, args).await?
        };
        let args = gated.arguments.as_ref().unwrap_or(args);
        if consequential_name(name) && consequential(name, args).is_none() {
            return Err("consequential calls require a valid operation_id and canonical arguments".into());
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
            let summary = summarize(name, args);
            let policy = self.policy.read().unwrap();
            let tool = policy.decide(&Request {
                channel: &ctx.channel,
                kind: Some(TOOL_KIND),
                title: name,
                command: Some(&summary),
                arguments: Some(args),
            });
            if tool.verdict == Verdict::Deny {
                return Err(refusal(tool));
            }
            let access = self.session.get().ok_or("authority requires a session")?;
            let decision = crate::authority::decide(
                access.store.as_ref(), &policy, &ctx.channel, &ctx.session_id,
                action, &target, revision.as_deref(), args,
            )?;
            match decision.verdict {
                Verdict::Allow => {}
                Verdict::Ask if gated.one_shot => {}
                Verdict::Deny => return Err(refusal(decision)),
                Verdict::Ask => return Err(format!(
                    "{action} for {target} needs a current scoped authority grant"
                )),
            }
            access.store.authority_action_set_grant(id, decision.rule_id.as_deref())
                .map_err(|e| e.to_string())
        };
        let result = match name {
            consulta::QUERY | consulta::DESCRIBE => consulta::call(&self.broker, &self.cfg, ctx, name, args).await,
            gitlab::MR_STATUS | gitlab::MR_COMMENT | gitlab::MR_MERGE | gitlab::PIPELINE_STATUS
            | gitlab::JOB_TRACE | gitlab::PIPELINE_RUN | gitlab::DEPLOY =>
                gitlab::call(&self.broker, &self.http, ctx, name, args, Some(&before_mutation)).await,
            jira::ISSUE | jira::ISSUE_SEARCH | jira::ISSUE_COMMENT | jira::ISSUE_UPDATE
            | jira::ISSUE_CREATE | jira::ISSUE_TRANSITION =>
                jira::call(&self.broker, &self.http, ctx, name, args, Some(&before_mutation)).await,
            review::SUBMIT | review::STATUS | review::VERDICT => {
                let access = self.session.get().ok_or("review tools are not available on this node")?;
                review::call(&access.store, &access.manager, ctx, name, args).await
            }
            memory::RECALL | memory::RETAIN => {
                let access = self.session.get().ok_or("memory is not available on this node")?;
                memory::call(self, access, ctx, name, args).await
            }
            work::WORK_READY | work::WORK_DISCOVER | work::WORK_CLOSE => {
                let access = self.session.get().ok_or_else(|| "node not ready".to_string())?;
                work::call(access, ctx, name, args).await
            }
            docs::DOC_READ | docs::DOC_SEARCH | docs::DOC_WRITE => {
                let access = self.session.get().ok_or("documents are not available on this node")?;
                docs::call(self, access, ctx, name, args).await
            }
            github::PR_STATUS | github::PR_COMMENT | github::RUN_STATUS | github::PR_MERGE =>
                github::call(&self.broker, &self.http, ctx, name, args, Some(&before_mutation)).await,
            other => Err(format!("no tool named {other}")),
        };
        let result = match (name, result) {
            (review::SUBMIT, Ok(submitted)) => self.auto_publish_review(ctx, args, submitted).await,
            (_, result) => result,
        };
        if let Some(ActionRecord::New(id)) = action_record {
            let (state, outcome) = match &result {
                Ok(value) => ("succeeded", value.to_string()),
                Err(error) if remote_outcome_unknown(error) => ("uncertain", error.clone()),
                Err(error) => ("failed", error.clone()),
            };
            self.session.get().expect("authority record requires session").store
                .authority_action_finish(&id, state, &outcome)
                .map_err(|e| e.to_string())?;
        }
        result
    }

    async fn auto_publish_review(
        &self,
        ctx: &CallContext,
        _args: &Value,
        submitted: Value,
    ) -> Result<Value, String> {
        let Some(review_id) = submitted.get("review_id").and_then(Value::as_str) else {
            return Ok(submitted);
        };
        let access = self.session.get().ok_or("review tools are not available")?;
        let review = access.store.get_review(review_id).map_err(|e| e.to_string())?
            .ok_or("review disappeared after capture")?;
        let target: crate::review::publish::Target =
            serde_json::from_str(&review.target).map_err(|e| e.to_string())?;
        let prose = crate::corpus::hash_body(&format!(
            "{}\u{1f}{}", review.approved_title(), review.approved_body()
        ));
        let canonical = format!(
            "publish:{}:{}:{}:{}:prose:{}",
            target.provider, target.project, target.base, target.branch, prose
        );
        let authority_args = serde_json::json!({
            "target": canonical.clone(),
            "revision": review.head_sha.clone(),
            "prose_hash": prose.clone(),
        });
        let decision = crate::authority::decide(
            access.store.as_ref(),
            &self.policy.read().unwrap(),
            &ctx.channel,
            &ctx.session_id,
            crate::authority::PUBLISH,
            authority_args["target"].as_str().expect("canonical target"),
            authority_args["revision"].as_str(),
            &authority_args,
        )?;
        if decision.verdict != Verdict::Allow {
            let mut submitted = submitted;
            if let Some(object) = submitted.as_object_mut() {
                object.insert("publication_authority".into(), serde_json::json!({
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
                }));
            }
            return Ok(submitted);
        }
        let action_id = uuid::Uuid::now_v7().to_string();
        access.store.authority_action_begin(
            &action_id, decision.rule_id.as_deref(), crate::authority::PUBLISH, &canonical,
            &ctx.channel, &ctx.session_id, Some(&review.head_sha), None,
            &serde_json::json!({ "review_id": review.id, "candidate": review.head_sha, "prose": prose }).to_string(),
        ).map_err(|e| e.to_string())?;
        let recheck = || {
            let current = crate::authority::decide(
                access.store.as_ref(), &self.policy.read().unwrap(), &ctx.channel, &ctx.session_id,
                crate::authority::PUBLISH, &canonical, Some(&review.head_sha), &authority_args,
            )?;
            if current.verdict != Verdict::Allow {
                return Err("publication authority was revoked or no longer applies".into());
            }
            access.store.authority_action_set_grant(&action_id, current.rule_id.as_deref())
                .map_err(|e| e.to_string())
        };
        match crate::authority::publish_review(
            access.store.as_ref(), &access.manager, &self.broker, &self.cfg,
            &ctx.node_id, &review, review.approved_title(), review.approved_body(), true, Some(&recheck),
        ).await {
            Ok(published) => {
                access.store.authority_action_finish(&action_id, "succeeded", &published)
                    .map_err(|e| e.to_string())?;
                Ok(serde_json::json!({
                    "review_id": review.id, "state": "approved", "published": published,
                    "authority": { "mode": "automatic", "id": decision.rule_id },
                }))
            }
            Err(error) => {
                access.store.authority_action_finish(&action_id, "failed", &error)
                    .map_err(|e| e.to_string())?;
                let state = access.store.get_review(&review.id).map_err(|e| e.to_string())?
                    .map(|current| current.state)
                    .unwrap_or_else(|| "missing".into());
                Ok(serde_json::json!({
                    "review_id": review.id, "state": state,
                    "authority": { "mode": "automatic", "outcome": "failed", "reason": error },
                }))
            }
        }
    }

    /// The definitions this caller is offered: the channel's tools, less what
    /// an attachment cannot use.
    pub fn list_offered(&self, ctx: &CallContext) -> Vec<Value> {
        let all = self.list(&ctx.channel, &ctx.node_id);
        if !self.is_external_session(ctx) {
            return all;
        }
        all.into_iter()
            .filter(|t| {
                !t["name"]
                    .as_str()
                    .is_some_and(|n| Self::NOT_EXTERNAL.contains(&n))
            })
            .collect()
    }

    fn is_external_session(&self, ctx: &CallContext) -> bool {
        self.session
            .get()
            .and_then(|a| a.store.get_session(&ctx.session_id).ok().flatten())
            .is_some_and(|s| s.harness_id == crate::session::external::HARNESS_ID)
    }

    fn is_review_session(&self, ctx: &CallContext) -> bool {
        self.session
            .get()
            .and_then(|a| a.store.get_session(&ctx.session_id).ok().flatten())
            .is_some_and(|s| s.phase == "review")
    }

    fn is_plan_artifact(&self, ctx: &CallContext, args: &Value) -> bool {
        let Some(access) = self.session.get() else {
            return false;
        };
        let Ok(Some(session)) = access.store.get_session(&ctx.session_id) else {
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
    async fn gate(
        &self,
        ctx: &CallContext,
        name: &str,
        args: &Value,
    ) -> Result<GatedCall, String> {
        let summary = summarize(name, args);
        let decision = self.decide(ctx, name, &summary, args);
        match decision.verdict {
            Verdict::Allow => Ok(GatedCall { arguments: None, one_shot: false }),
            Verdict::Deny => Err(refusal(decision)),
            Verdict::Ask => {
                let access = self
                    .session
                    .get()
                    .ok_or("this call needs the operator's approval and no session can ask")?;
                let request = PermissionRequest {
                    tool_call_id: None,
                    title: summary,
                    kind: Some(TOOL_KIND.into()),
                    raw_input: Some(json!({ "tool": name, "arguments": args })),
                    options: vec![
                        PermissionOption {
                            option_id: OPTION_ALLOW_ONCE.into(),
                            name: "Allow once".into(),
                            kind: "allow_once".into(),
                        },
                        PermissionOption {
                            option_id: OPTION_REJECT_ONCE.into(),
                            name: "Reject".into(),
                            kind: "reject_once".into(),
                        },
                    ],
                };
                match access
                    .manager
                    .ask_permission(&ctx.session_id, request)
                    .await
                    .map_err(|e| e.to_string())?
                {
                    PermissionReply::Selected(o) if o == OPTION_ALLOW_ONCE => Ok(GatedCall {
                        arguments: None,
                        one_shot: true,
                    }),
                    // What runs is the operator's rewrite, and a refusal is
                    // final whoever wrote the words.
                    PermissionReply::Edited {
                        option_id,
                        arguments,
                    } if option_id == OPTION_ALLOW_ONCE => {
                        let edited =
                            self.decide(ctx, name, &summarize(name, &arguments), &arguments);
                        if edited.verdict == Verdict::Deny {
                            return Err(refusal(edited));
                        }
                        Ok(GatedCall {
                            arguments: Some(arguments),
                            one_shot: true,
                        })
                    }
                    _ => Err("the operator did not allow this call".into()),
                }
            }
        }
    }

    fn decide(&self, ctx: &CallContext, name: &str, summary: &str, args: &Value) -> Decision {
        let tool = self.policy.read().unwrap().decide(&Request {
            channel: &ctx.channel,
            kind: Some(TOOL_KIND),
            title: name,
            command: Some(summary),
            arguments: Some(args),
        });
        if tool.verdict == Verdict::Deny {
            return tool;
        }
        if let Some((action, target, revision)) = consequential(name, args) {
            let Some(access) = self.session.get() else {
                return Decision { verdict: Verdict::Ask, rule_id: None, reason: None };
            };
            return crate::authority::decide(
                access.store.as_ref(),
                &self.policy.read().unwrap(),
                &ctx.channel,
                &ctx.session_id,
                action,
                &target,
                revision.as_deref(),
                args,
            )
            .unwrap_or(Decision { verdict: Verdict::Ask, rule_id: None, reason: None });
        }
        tool
    }

    fn revalidate_tool_policy(&self, ctx: &CallContext, name: &str, args: &Value) -> Result<(), String> {
        let summary = summarize(name, args);
        let decision = self.policy.read().unwrap().decide(&Request {
            channel: &ctx.channel,
            kind: Some(TOOL_KIND),
            title: name,
            command: Some(&summary),
            arguments: Some(args),
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
            &self.policy.read().unwrap(),
            &ctx.channel,
            &ctx.session_id,
            action,
            &target,
            revision.as_deref(),
            args,
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
            access.store.as_ref(), &self.policy.read().unwrap(), &ctx.channel, &ctx.session_id,
            action, &target, revision.as_deref(), args,
        )?;
        if decision.verdict != Verdict::Allow && !(one_shot && decision.verdict == Verdict::Ask) {
            return Err("authority changed before dispatch".into());
        }
        let evidence = serde_json::json!({
            "arguments": canonical_consequential_payload(name, args).expect("validated consequential arguments"),
        }).to_string();
        let operation_id = args["operation_id"].as_str().expect("validated operation id");
        let request_hash = crate::corpus::hash_body(&evidence);
        if let Some((state, outcome, recorded_hash)) = access.store.authority_action_existing(
            action, &target, &ctx.channel, revision.as_deref(), operation_id,
        ).map_err(|e| e.to_string())? {
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
        if let Err(error) = access.store.authority_action_begin(
            &id, decision.rule_id.as_deref(), action, &target, &ctx.channel, &ctx.session_id,
            revision.as_deref(), Some(operation_id), &evidence,
        ) {
            if let Some((state, outcome, recorded_hash)) = access.store.authority_action_existing(
                action, &target, &ctx.channel, revision.as_deref(), operation_id,
            ).map_err(|e| e.to_string())? {
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
            "initialize" => Ok(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "tracon", "version": env!("CARGO_PKG_VERSION") },
            })),
            "tools/list" => Ok(json!({ "tools": self.list_offered(ctx) })),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                self.call(ctx, name, &args).await.map(|v| tool_result(&v, false))
                    .or_else(|e| Ok(tool_result(&json!(e), true)))
            }
            "ping" => Ok(json!({})),
            other => Err(format!("unsupported method {other}")),
        };
        Some(match result {
            Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
            Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": e } }),
        })
    }
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
    let string = |key| args.get(key).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty());
    if matches!(name, github::PR_MERGE | gitlab::MR_MERGE | gitlab::DEPLOY | jira::ISSUE_TRANSITION)
        && !string("operation_id").is_some_and(|id| {
            id.len() >= 8 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
    {
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
            format!("gitlab:{}:mr:{}", string("project")?, args.get("iid").and_then(Value::as_i64)?),
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
            if !transition.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')) {
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

/// Normalize provider defaults and ignore arguments a consequential provider
/// does not consume before comparing a stable operation id on replay.
fn canonical_consequential_payload(name: &str, args: &Value) -> Option<Value> {
    let string = |key| args.get(key).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty());
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
/// the JSON, so a deny rule can name one.
pub fn summarize(name: &str, args: &Value) -> String {
    let mut s = match args.get("profile").and_then(Value::as_str) {
        Some(p) => format!("{name} profile={p} {args}"),
        None => format!("{name} {args}"),
    };
    if s.len() > 400 {
        let mut cut = 400;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push('…');
    }
    s
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
        CallContext {
            session_id: "s".into(),
            channel: channel.into(),
            node_id: "n1".into(),
        }
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
        t.policy = Arc::new(std::sync::RwLock::new(
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
        t.policy = Arc::new(std::sync::RwLock::new(
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
