//! The tools the node exposes to a harness, over MCP.
//!
//! The transport is the gateway's forward: the harness can reach the node and
//! nothing else, and it carries a token minted per session. A tool is the only
//! shape a credential ever reaches the harness in — as something it may ask the
//! node to do, never as something it holds.

pub mod consulta;
pub mod docs;
pub mod gitlab;
pub mod jira;
pub mod memory;
pub mod review;
pub mod work;

use std::sync::Arc;

use serde_json::{json, Value};

use crate::{
    acp::types::{PermissionOption, OPTION_ALLOW_ONCE, OPTION_REJECT_ONCE},
    adapter::{PermissionReply, PermissionRequest},
    broker::SharedBroker,
    config::Config,
    policy::{Policy, Request, Verdict},
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

impl Tools {
    /// The tools a review session gets: what it needs to read, and its
    /// verdict. Nothing that writes, publishes, or reaches a forge.
    pub const REVIEW_TOOLS: &'static [&'static str] = &[
        memory::RECALL,
        docs::DOC_READ,
        docs::DOC_SEARCH,
        review::VERDICT,
    ];

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
        if available.contains(&consulta::CREDENTIAL) {
            out.extend(consulta::definitions());
        }
        if available.contains(&gitlab::CREDENTIAL) {
            out.extend(gitlab::definitions());
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
        if !plan_write {
            self.gate(ctx, name, args).await?;
        }
        match name {
            consulta::QUERY | consulta::DESCRIBE => {
                consulta::call(&self.broker, &self.cfg, ctx, name, args).await
            }
            gitlab::MR_STATUS | gitlab::MR_COMMENT => {
                gitlab::call(&self.broker, &self.http, ctx, name, args).await
            }
            jira::ISSUE | jira::ISSUE_COMMENT => {
                jira::call(&self.broker, &self.http, ctx, name, args).await
            }
            review::SUBMIT | review::STATUS | review::VERDICT => {
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
                memory::call(access, ctx, name, args).await
            }
            work::WORK_READY | work::WORK_DISCOVER | work::WORK_CLOSE => {
                let access = self
                    .session
                    .get()
                    .ok_or_else(|| "node not ready".to_string())?;
                work::call(access, ctx, name, args).await
            }
            docs::DOC_READ | docs::DOC_SEARCH | docs::DOC_WRITE => {
                let access = self
                    .session
                    .get()
                    .ok_or("documents are not available on this node")?;
                docs::call(access, ctx, name, args).await
            }
            other => Err(format!("no tool named {other}")),
        }
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
    /// run — adding a tool never widens what runs unattended.
    async fn gate(&self, ctx: &CallContext, name: &str, args: &Value) -> Result<(), String> {
        let summary = summarize(name, args);
        let decision = self.policy.read().unwrap().decide(&Request {
            channel: &ctx.channel,
            kind: Some(TOOL_KIND),
            title: name,
            command: Some(&summary),
        });
        match decision.verdict {
            Verdict::Allow => Ok(()),
            Verdict::Deny => Err(format!(
                "refused by policy{}: {}",
                decision
                    .rule_id
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default(),
                decision.reason.unwrap_or_default()
            )),
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
                    PermissionReply::Selected(o) if o == OPTION_ALLOW_ONCE => Ok(()),
                    _ => Err("the operator did not allow this call".into()),
                }
            }
        }
    }

    /// Handle one MCP JSON-RPC message. Returns `None` for notifications, which
    /// take no response.
    pub async fn handle(&self, ctx: &CallContext, msg: &Value) -> Option<Value> {
        // Notifications carry no id and expect nothing back.
        let id = msg.get("id").cloned()?;
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "tracon", "version": env!("CARGO_PKG_VERSION") },
            })),
            "tools/list" => Ok(json!({ "tools": self.list(&ctx.channel, &ctx.node_id) })),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                match self.call(ctx, name, &args).await {
                    Ok(v) => Ok(tool_result(&v, false)),
                    // A refused tool is a result the model can read and act on,
                    // not a protocol error it cannot see.
                    Err(e) => Ok(tool_result(&json!(e), true)),
                }
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

/// `name` plus its arguments on one line, bounded, for the policy haystack
/// and the queue card. Secrets never appear here: arguments are the
/// harness's own words.
pub fn summarize(name: &str, args: &Value) -> String {
    let mut s = format!("{name} {}", args);
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
