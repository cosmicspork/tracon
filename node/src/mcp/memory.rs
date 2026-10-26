//! Memory as the node owns it: `recall` reads across memories and documents,
//! `retain` writes what the agent learned. Directives are human-only; a
//! lesson, or a fact the agent is not sure of, enters as `proposed` and waits
//! for the nightly batch rather than becoming context on its own. So does a
//! correction: a memory retained with `supersedes` replaces the one it names
//! only when the operator promotes it.

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
                            what happened and is only ever searched. Directives are the operator's. \
                            When a recalled memory is wrong or stale, pass its id as supersedes: the \
                            correction waits for the operator and, once promoted, replaces it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": ["fact", "lesson", "episode"] },
                    "scope": { "type": "string", "enum": ["project", "session", "global"], "default": "project" },
                    "body": { "type": "string" },
                    "confidence": { "type": "number", "minimum": 0, "maximum": 1, "default": 0.8 },
                    "supersedes": { "type": "string", "description": "The id of a memory (from recall) this one corrects. Without a scope, the correction takes that memory's." },
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
    let project_id = ctx
        .session_id()
        .and_then(|id| access.store.get_session(id).ok().flatten())
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
                    ctx.session_id(),
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
            // A correction names a live memory this channel holds (what recall
            // returns), and never one of the operator's directives: refused
            // here, before anything is written.
            let target =
                match args["supersedes"].as_str().map(str::trim) {
                    None | Some("") => None,
                    Some(old) => {
                        let row = access
                            .store
                            .memory_get(old)
                            .map_err(|e| e.to_string())?
                            .filter(|m| {
                                m.deleted == 0
                                    && m.channel == ctx.channel
                                    && matches!(m.state.as_str(), "active" | "promoted")
                            });
                        match row {
                            None => {
                                return Err(format!(
                                    "no live memory {old:?} on this channel to supersede"
                                ))
                            }
                            Some(m) if m.kind == KIND_DIRECTIVE => return Err(
                                "directives are the operator's; a correction cannot replace one"
                                    .into(),
                            ),
                            Some(m) => Some(m),
                        }
                    }
                };
            let supersedes = target.as_ref().map(|m| m.id.clone());
            // A correction reaches as far as what it replaces unless told
            // otherwise, so promoting it never quietly narrows a fact.
            let inherited = match (args["scope"].as_str(), &target) {
                (None, Some(m)) => Some((m.scope.clone(), m.scope_ref.clone())),
                _ => None,
            };
            let scope = match &inherited {
                Some((scope, _)) => scope.as_str(),
                None => args["scope"].as_str().unwrap_or("project"),
            };
            let scope_ref = match scope {
                _ if inherited.is_some() => inherited.as_ref().and_then(|(_, r)| r.clone()),
                "project" => match &project_id {
                    Some(p) => Some(p.clone()),
                    None if ctx.is_external() => {
                        return Err(
                            "a harness you run yourself has no project identity; use scope global"
                                .into(),
                        )
                    }
                    None => {
                        return Err(
                            "this session has no project identity; use scope session or global"
                                .into(),
                        )
                    }
                },
                "session" => match ctx.session_id() {
                    Some(id) => Some(id.to_string()),
                    None => {
                        return Err(
                            "a harness you run yourself has no session scope; use global (this channel) or project"
                                .into(),
                        )
                    }
                },
                "global" => None,
                other => return Err(format!("unknown scope {other:?}")),
            };
            let confidence = args["confidence"].as_f64().unwrap_or(0.8).clamp(0.0, 1.0);
            // What becomes context on its own is bounded: a lesson always waits
            // for the operator, a fact only when the agent is sure of it, and a
            // correction always, since promoting it retires what it replaces.
            let state = match kind {
                _ if supersedes.is_some() => "candidate",
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
                    "source_session": ctx.session_id(), "source_node": node_id, "confidence": confidence,
                    "state": state, "supersedes": supersedes, "created_ms": now, "updated_ms": now,
                }),
            )
            .map_err(|e| e.to_string())?;
            let note = match (&supersedes, state) {
                (Some(_), _) => "held for the operator's nightly batch; promoting it retires the memory it supersedes, which recall marks as having a correction pending until then",
                (None, "candidate") => "held for the operator's nightly batch; it is not context until promoted",
                _ => "active: recalled by later sessions on this channel",
            };
            let mut out = json!({ "id": id, "state": state, "note": note });
            if let Some(old) = supersedes {
                out["supersedes"] = json!(old);
            }
            Ok(out)
        }
        other => Err(format!("no tool named {other}")),
    }
}
