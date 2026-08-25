//! The tools the node exposes to a harness, over MCP.
//!
//! The transport is the gateway's forward: the harness can reach the node and
//! nothing else, and it carries a token minted per session. A tool is the only
//! shape a credential ever reaches the harness in — as something it may ask the
//! node to do, never as something it holds.

pub mod consulta;
pub mod review;

use std::sync::Arc;

use serde_json::{json, Value};

use crate::{broker::Broker, config::Config};

/// The MCP protocol version the node speaks.
const PROTOCOL_VERSION: &str = "2025-06-18";

pub struct Tools {
    pub broker: Arc<Broker>,
    pub cfg: Arc<Config>,
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
}

impl Tools {
    /// Tool definitions for a channel. A channel with no credential bound to it
    /// is offered no tools rather than tools that will fail.
    pub fn list(&self, channel: &str) -> Vec<Value> {
        let mut out = Vec::new();
        if self
            .broker
            .available_to(channel)
            .contains(&consulta::CREDENTIAL)
        {
            out.extend(consulta::definitions());
        }
        // Review tools need no credential of their own: submitting is always
        // allowed, and publishing is what needs one. An agent that cannot
        // publish can still ask for review and be told why it stopped there.
        if self.session.get().is_some() {
            out.extend(review::definitions());
        }
        out
    }

    pub async fn call(&self, ctx: &CallContext, name: &str, args: &Value) -> Result<Value, String> {
        match name {
            consulta::QUERY | consulta::DESCRIBE => {
                consulta::call(&self.broker, &self.cfg, ctx, name, args).await
            }
            review::SUBMIT | review::STATUS => {
                let access = self
                    .session
                    .get()
                    .ok_or("review tools are not available on this node")?;
                review::call(&access.store, &access.manager, ctx, name, args).await
            }
            other => Err(format!("no tool named {other}")),
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
            "tools/list" => Ok(json!({ "tools": self.list(&ctx.channel) })),
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
            broker: Arc::new(toml::from_str(store).unwrap()),
            cfg: Arc::new(Config::default()),
            session: Default::default(),
        }
    }

    fn ctx(channel: &str) -> CallContext {
        CallContext {
            session_id: "s".into(),
            channel: channel.into(),
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
        assert_eq!(t.list("work").len(), 2);
        assert!(t.list("personal").is_empty());
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
