//! Memory as the node owns it: `recall` reads across memories and documents,
//! `retain` writes what the agent learned. Directives are human-only; a
//! lesson, or a fact the agent is not sure of, enters as `proposed` and waits
//! for the nightly batch rather than becoming context on its own.

use serde_json::{json, Value};
use tracon_sync::ChangeOp;

use crate::{
    corpus,
    mcp::{CallContext, SessionAccess},
    store::{now_ms, KIND_DIRECTIVE, KIND_EPISODE, KIND_FACT, KIND_LESSON},
};

pub const RECALL: &str = "recall";
pub const RETAIN: &str = "retain";

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": RECALL,
            "description": "Search what is known: the operator's directives first, then facts about \
                            this project, promoted lessons, and documents (returned as slug + snippet; \
                            call doc_read for the whole thing). Scope narrows to this session, its \
                            project, the client, and the channel. Episodes only when asked for.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "kinds": { "type": "array", "items": { "type": "string", "enum": ["directive", "fact", "lesson", "episode", "document"] } },
                    "limit": { "type": "integer", "default": 8 },
                },
                "required": ["query"],
            },
        }),
        json!({
            "name": RETAIN,
            "description": "Remember something for later sessions. A fact is a durable truth about \
                            the code (say how sure you are); a lesson is a generalised gotcha and is \
                            proposed to the operator before it is ever injected; an episode records \
                            what happened and is only ever searched. Directives are the operator's.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": ["fact", "lesson", "episode"] },
                    "scope": { "type": "string", "enum": ["project", "session", "global"], "default": "project" },
                    "body": { "type": "string" },
                    "confidence": { "type": "number", "minimum": 0, "maximum": 1, "default": 0.8 },
                },
                "required": ["kind", "body"],
            },
        }),
    ]
}

pub async fn call(
    tools: &super::Tools,
    access: &SessionAccess,
    ctx: &CallContext,
    name: &str,
    args: &Value,
) -> Result<Value, String> {
    let project_id = access
        .store
        .get_session(&ctx.session_id)
        .ok()
        .flatten()
        .and_then(|s| s.project_id);
    match name {
        RECALL => {
            let query = args["query"].as_str().unwrap_or("").trim();
            if query.is_empty() {
                return Err("recall needs a query".into());
            }
            let kinds: Option<Vec<String>> = args["kinds"].as_array().map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            });
            let limit = args["limit"].as_u64().unwrap_or(8).clamp(1, 50) as usize;
            let near = crate::embed::neighbours(
                &tools.cfg,
                &access.store,
                &tools.http,
                access.manager.probe_token(),
                Some(&ctx.channel),
                query,
                limit,
            )
            .await;
            let hits = access
                .store
                .recall_hybrid(
                    &ctx.channel,
                    query,
                    project_id.as_deref(),
                    Some(&ctx.session_id),
                    kinds.as_deref(),
                    limit,
                    &near.hits,
                )
                .map_err(|e| e.to_string())?;
            Ok(json!({ "hits": hits }))
        }
        RETAIN => {
            let kind = args["kind"].as_str().unwrap_or("");
            if kind == KIND_DIRECTIVE {
                return Err(
                    "directives are the operator's; retain a fact or a lesson instead".into(),
                );
            }
            if ![KIND_FACT, KIND_LESSON, KIND_EPISODE].contains(&kind) {
                return Err(format!("unknown memory kind {kind:?}"));
            }
            let body = args["body"].as_str().unwrap_or("").trim();
            if body.is_empty() {
                return Err("retain needs a body".into());
            }
            let scope = args["scope"].as_str().unwrap_or("project");
            let scope_ref = match scope {
                "project" => match &project_id {
                    Some(p) => Some(p.clone()),
                    None => {
                        return Err(
                            "this session has no project identity; use scope session or global"
                                .into(),
                        )
                    }
                },
                "session" => Some(ctx.session_id.clone()),
                "global" => None,
                other => return Err(format!("unknown scope {other:?}")),
            };
            let confidence = args["confidence"].as_f64().unwrap_or(0.8).clamp(0.0, 1.0);
            // What becomes context on its own is bounded: a lesson always waits
            // for the operator, a fact only when the agent is sure of it.
            let state = match kind {
                KIND_LESSON => "candidate",
                KIND_FACT if confidence < crate::store::CONFIDENT => "candidate",
                _ => "active",
            };
            let id = corpus::new_id();
            let now = now_ms();
            let node_id = access.manager.node_id().to_string();
            corpus::write(
                &access.store,
                access.manager.bus(),
                &node_id,
                &ctx.channel,
                "memory",
                ChangeOp::Upsert,
                &id,
                json!({
                    "channel": ctx.channel, "scope": scope, "scope_ref": scope_ref, "kind": kind, "body": body,
                    "source_session": ctx.session_id, "source_node": node_id, "confidence": confidence,
                    "state": state, "created_ms": now, "updated_ms": now,
                }),
            )
            .map_err(|e| e.to_string())?;
            Ok(
                json!({ "id": id, "state": state, "note": if state == "candidate" {
                "held for the operator's nightly batch; it is not context until promoted"
            } else { "active: recalled by later sessions on this channel" } }),
            )
        }
        other => Err(format!("no tool named {other}")),
    }
}
