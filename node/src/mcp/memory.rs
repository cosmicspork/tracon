//! Memory as the node owns it: `recall` reads across memories and documents,
//! `retain` writes what the agent learned. Directives are human-only; a
//! lesson, or a fact the agent is not sure of, enters as `proposed` and waits
//! for the nightly batch rather than becoming context on its own. So does a
//! correction: a memory retained with `supersedes` replaces the one it names
//! only when the operator promotes it. A small wording fix to a memory that
//! stands is `memory_amend`, which rewrites it in place and so is always put
//! to the operator first.

use serde_json::{json, Value};
use tracon_sync::ChangeOp;

use crate::{
    corpus,
    mcp::{CallContext, SessionAccess},
    store::{
        now_ms, MemoryRow, Store, StoreError, KIND_DIRECTIVE, KIND_EPISODE, KIND_FACT, KIND_LESSON,
    },
};

pub const RECALL: &str = "recall";
pub const RETAIN: &str = "retain";
pub const AMEND: &str = "memory_amend";

/// How a harness outside the boundary says which project it means: it has no
/// session to have been launched on one.
const WORKTREE: &str = "Only for a harness you run yourself: the absolute path of a worktree of \
                        the repository you mean, which gives you its project scope. Its \
                        repository must be under [external] repo_roots.";

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
                    "worktree": { "type": "string", "description": WORKTREE },
                },
                "required": ["query"],
            },
        }),
        json!({
            "name": RETAIN,
            "description": "Remember something for later sessions. A fact is a durable truth about \
                            the code (say how sure you are); a lesson is a generalised gotcha and is \
                            proposed to the operator before it is ever injected; an episode records \
                            what happened and is only ever searched. A confident project fact is told \
                            to every later session on that project; a global one belongs to the \
                            whole channel and is only recalled. Directives are the operator's. \
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
                    "worktree": { "type": "string", "description": WORKTREE },
                },
                "required": ["kind", "body"],
            },
        }),
        json!({
            "name": AMEND,
            "description": "Fix the wording of a recalled memory in place: a typo, a stale name, a \
                            sentence that misleads. The operator is always asked, and sees your body \
                            as a diff against the memory as it stands; nothing changes until they \
                            allow it. Only the body changes: kind and scope stay. When the memory is \
                            wrong rather than badly worded, retain a correction with supersedes \
                            instead. Directives are the operator's. An asked call returns \
                            `awaiting_operator` with an `approval_id` at once, and approval_status \
                            reports the outcome.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The id of the memory (from recall) to amend." },
                    "body": { "type": "string", "description": "The whole amended body, not just the changed words." },
                    "reason": { "type": "string", "description": "Why the wording should change, for the operator." },
                },
                "required": ["id", "body", "reason"],
            },
        }),
    ]
}

/// Whether `m` is a memory an agent on `channel` may point at: one recall
/// returns there, live and not deleted.
fn live_on(m: &MemoryRow, channel: &str) -> bool {
    m.deleted == 0 && m.channel == channel && matches!(m.state.as_str(), "active" | "promoted")
}

/// The memory a `memory_amend` rewrites, or why it cannot: the same rules as a
/// correction's target (a live memory on this channel, never a directive),
/// plus a body and a reason that say something. Checked before the call is
/// held, so the operator is never asked about one that cannot run, and again
/// when it runs, since the memory may have changed in between.
pub fn amend_target(store: &Store, channel: &str, args: &Value) -> Result<MemoryRow, String> {
    let id = args["id"].as_str().unwrap_or("").trim();
    if id.is_empty() {
        return Err("memory_amend needs the id of the memory to amend".into());
    }
    let body = args["body"].as_str().unwrap_or("").trim();
    if body.is_empty() {
        return Err("memory_amend needs the amended body".into());
    }
    if args["reason"].as_str().unwrap_or("").trim().is_empty() {
        return Err("memory_amend needs a reason the operator can weigh".into());
    }
    let m = store
        .memory_get(id)
        .map_err(|e| e.to_string())?
        .filter(|m| live_on(m, channel))
        .ok_or_else(|| format!("no live memory {id:?} on this channel to amend"))?;
    if m.kind == KIND_DIRECTIVE {
        return Err("directives are the operator's; an agent cannot amend one".into());
    }
    if m.body.trim() == body {
        return Err("the amended body is the memory as it stands; nothing to change".into());
    }
    Ok(m)
}

/// What the approval page shows a held `memory_amend` against: the memory as
/// it stands now, or `null` when it is no longer one the call could amend.
pub fn approval_context(store: &Store, channel: &str, args: &Value) -> Result<Value, StoreError> {
    let id = args["id"].as_str().unwrap_or("").trim();
    let m = store.memory_get(id)?.filter(|m| live_on(m, channel));
    Ok(json!({
        "memory": m.map(|m| json!({
            "id": m.id,
            "kind": m.kind,
            "scope": m.scope,
            "scope_ref": m.scope_ref,
            "body": m.body,
            "state": m.state,
            "updated_ms": m.updated_ms,
        })),
    }))
}

/// Run an amendment the operator allowed: the body is replaced in place, as
/// the operator's own edit (`PATCH /api/memories/{id}`) replaces it, and
/// nothing else about the memory changes.
pub fn amend(access: &SessionAccess, ctx: &CallContext, args: &Value) -> Result<Value, String> {
    let m = amend_target(&access.store, &ctx.channel, args)?;
    let body = args["body"].as_str().unwrap_or("").trim();
    corpus::write(
        &access.store,
        access.manager.bus(),
        access.manager.node_id(),
        &m.channel,
        "memory",
        ChangeOp::Upsert,
        &m.id,
        m.with_body(body, now_ms()),
    )
    .map_err(|e| e.to_string())?;
    Ok(json!({
        "id": m.id,
        "state": m.state,
        "note": "amended: later sessions recall the new wording",
    }))
}

pub async fn call(
    tools: &super::Tools,
    access: &SessionAccess,
    ctx: &CallContext,
    name: &str,
    args: &Value,
) -> Result<Value, String> {
    let worktree = args["worktree"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    // A session's project is the one it was launched on; a harness outside
    // the boundary names one by a worktree of it, as `work_discover` does.
    let project_id = match (ctx.is_external(), worktree) {
        (false, Some(_)) => {
            return Err("worktree is for a harness you run yourself; \
                        this session's project is the one it was launched on"
                .into())
        }
        (true, Some(path)) => Some(
            super::work::external_project(&tools.cfg, &access.store, &ctx.channel, path).await?,
        ),
        (true, None) => None,
        (false, None) => ctx
            .session_id()
            .and_then(|id| access.store.get_session(id).ok().flatten())
            .and_then(|s| s.project_id),
    };
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
                            "a harness you run yourself names its project with worktree; \
                             without one, use scope global"
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
