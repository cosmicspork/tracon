//! The ledger as the agent sees it: `work_ready` lists what is unblocked
//! (the tool sorts, the model picks), `work_discover` records work found
//! mid-session linked to its origin instead of evaporating with the
//! session, and `work_close` closes the item this session holds — or, for a
//! harness the operator runs, which holds none, the item it names.
//!
//! `brief_read` and `brief_note` are the item's product brief, when it has
//! one: who the work is for and what would make it good, as the operator
//! recorded it. Reading is free; writing a line is asked, and the node holds
//! a session to what a session can honestly claim — never a decision, and
//! never an observation with nothing to point at.

use serde_json::{json, Value};

use crate::{
    corpus,
    mcp::{CallContext, SessionAccess},
};

pub const WORK_READY: &str = "work_ready";
pub const WORK_DISCOVER: &str = "work_discover";
pub const WORK_CLOSE: &str = "work_close";
pub const BRIEF_READ: &str = "brief_read";
pub const BRIEF_NOTE: &str = "brief_note";

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": WORK_READY,
            "description": "Ready work on this channel: open items whose dependencies are all closed, \
                            in the order the node computes (priority, then age). Items another session \
                            holds are omitted.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "default": 10 },
                },
            },
        }),
        json!({
            "name": WORK_DISCOVER,
            "description": "Record work you found but are not doing now. It is linked to this session's \
                            item as discovered-from, so it survives the session. Say what and why; add \
                            deps if it must wait on other items.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": { "type": "string" },
                    "body": { "type": "string" },
                    "deps": { "type": "array", "items": { "type": "string" } },
                    "priority": { "type": "integer", "default": 0 },
                },
                "required": ["title"],
            },
        }),
        json!({
            "name": BRIEF_READ,
            "description": "The product brief of the item this session holds, when it has one: intended \
                            user, problem, source references, constraints, success criteria and open \
                            questions. Every line says whether the customer was observed saying it, \
                            someone inferred it, or the operator decided it, and what it points at. \
                            An item without a brief is normal; the tool says so.",
            "inputSchema": { "type": "object", "properties": {} },
        }),
        json!({
            "name": BRIEF_NOTE,
            "description": "Add a line to the brief of the item this session holds. Say which section, \
                            and point at what the line rests on. You may record `inferred` (you reasoned \
                            to it) or `observed` (the customer said or did it) — an observation must cite \
                            a doc, url, evidence id or session. You may not record a decision: that is \
                            the operator's. The line is written with a reference to this session.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "field": {
                        "type": "string",
                        "enum": ["intended_user", "problem", "source_references", "constraints", "success_criteria", "unresolved_questions"],
                    },
                    "provenance": { "type": "string", "enum": ["observed", "inferred"], "default": "inferred" },
                    "text": { "type": "string" },
                    "refs": {
                        "type": "array",
                        "description": "What the line rests on, as {kind, value} with kind one of doc, session, evidence, work, url, file.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "kind": { "type": "string", "enum": crate::corpus::brief::REF_KINDS },
                                "value": { "type": "string" },
                            },
                            "required": ["kind", "value"],
                        },
                    },
                },
                "required": ["field", "text"],
            },
        }),
        json!({
            "name": WORK_CLOSE,
            "description": "Close a work item because it is done. In a session the node started \
                            that is the session's own item, and the session ends with it. A \
                            harness you run yourself passes the item's `id`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The item to close. Only for a harness you run yourself." },
                    "summary": { "type": "string" },
                },
            },
        }),
    ]
}

pub async fn call(
    access: &SessionAccess,
    ctx: &CallContext,
    name: &str,
    args: &Value,
) -> Result<Value, String> {
    let session = access
        .store
        .get_session(&ctx.session_id)
        .map_err(|e| e.to_string())?;
    let (project_id, item_id) = session
        .as_ref()
        .map(|s| (s.project_id.clone(), s.work_item_id.clone()))
        .unwrap_or((None, None));
    let external = session
        .as_ref()
        .is_some_and(|s| s.harness_id == crate::session::external::HARNESS_ID);
    match name {
        WORK_READY => {
            let limit = args["limit"].as_u64().unwrap_or(10).clamp(1, 50) as usize;
            let items = access
                .store
                .work_ready(&ctx.channel, project_id.as_deref())
                .map_err(|e| e.to_string())?;
            let out: Vec<Value> = items
                .iter()
                .take(limit)
                .map(|v| {
                    json!({
                        "id": v.item.id, "title": v.item.title, "priority": v.item.priority,
                        "body": v.item.body, "deps": v.item.deps,
                    })
                })
                .collect();
            Ok(json!({ "items": out }))
        }
        WORK_DISCOVER => {
            let title = args["title"].as_str().unwrap_or("").trim();
            if title.is_empty() {
                return Err("work_discover needs a title".into());
            }
            let deps: Vec<String> = args["deps"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let item = corpus::work::create(
                &access.store,
                access.manager.bus(),
                &ctx.node_id,
                corpus::work::NewWork {
                    channel: ctx.channel.clone(),
                    project_id,
                    title: title.to_string(),
                    body: args["body"].as_str().unwrap_or("").to_string(),
                    deps,
                    priority: args["priority"].as_i64().unwrap_or(0),
                    discovered_from: item_id,
                    discovered_by_session: Some(ctx.session_id.clone()),
                },
            )
            .map_err(|e| e.to_string())?;
            Ok(
                json!({ "id": item.id, "title": item.title, "discovered_from": item.discovered_from }),
            )
        }
        WORK_CLOSE if external => {
            let id = args["id"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or("work_close needs the item's id")?;
            // Closing an item ends the session working on it; that is not a
            // call a harness outside the boundary makes for someone else.
            let held = access
                .store
                .list_sessions(None)
                .map_err(|e| e.to_string())?
                .into_iter()
                .find(|s| s.work_item_id.as_deref() == Some(id) && s.state != "closed");
            if let Some(s) = held {
                return Err(format!(
                    "session {} is working on {id}; let it finish, or kill it first",
                    s.id
                ));
            }
            let item = corpus::work::close(
                &access.store,
                access.manager.bus(),
                &ctx.node_id,
                id,
                Some(&ctx.session_id),
            )
            .map_err(|e| e.to_string())?;
            access.manager.record_event(
                &ctx.session_id,
                crate::session::state::event_kind::WORK_CLOSED,
                json!({ "item": item.id, "summary": args["summary"].as_str().unwrap_or("") }),
            );
            Ok(json!({ "id": item.id, "state": item.state }))
        }
        WORK_CLOSE => {
            let Some(id) = item_id else {
                return Err("this session holds no work item".into());
            };
            let item = corpus::work::close(
                &access.store,
                access.manager.bus(),
                &ctx.node_id,
                &id,
                Some(&ctx.session_id),
            )
            .map_err(|e| e.to_string())?;
            access
                .manager
                .item_closed(&ctx.session_id, args["summary"].as_str().unwrap_or(""))
                .await;
            Ok(json!({ "id": item.id, "state": item.state }))
        }
        BRIEF_READ => {
            let Some(id) = item_id else {
                return Err("this session holds no work item, so it has no brief".into());
            };
            let view = corpus::brief::for_item(&access.store, &id).map_err(|e| e.to_string())?;
            match view {
                Some(view) => {
                    let summary = corpus::brief::summary(&view);
                    Ok(json!({ "brief": view, "summary": summary }))
                }
                None => Ok(json!({
                    "brief": null,
                    "summary": format!("{} has no brief; work from the item and the plan", &id[..12.min(id.len())]),
                })),
            }
        }
        BRIEF_NOTE => {
            let Some(id) = item_id else {
                return Err("this session holds no work item, so it has no brief".into());
            };
            let field = args["field"].as_str().unwrap_or("").trim().to_string();
            let text = args["text"].as_str().unwrap_or("").trim().to_string();
            if text.is_empty() {
                return Err("brief_note needs the line's text".into());
            }
            let refs: Vec<corpus::brief::RefInput> =
                serde_json::from_value(args.get("refs").cloned().unwrap_or_else(|| json!([])))
                    .map_err(|e| format!("refs must be {{kind, value}} objects: {e}"))?;
            let view = corpus::brief::append_for_item(
                &access.store,
                access.manager.bus(),
                &ctx.node_id,
                &id,
                vec![corpus::brief::SectionInput {
                    field,
                    notes: None,
                    replace: false,
                    entries: Some(vec![corpus::brief::EntryInput {
                        provenance: args["provenance"].as_str().map(str::to_string),
                        text,
                        refs,
                    }]),
                }],
                &corpus::brief::Author::Session(ctx.session_id.clone()),
            )
            .map_err(|e| e.to_string())?;
            Ok(json!({ "slug": view.slug, "summary": corpus::brief::summary(&view) }))
        }
        other => Err(format!("no work tool named {other}")),
    }
}
