//! The ledger as the agent sees it: `work_ready` lists what is unblocked
//! (the tool sorts, the model picks), `work_discover` records work found
//! mid-session linked to its origin instead of evaporating with the
//! session, and `work_close` closes the item this session holds.

use serde_json::{json, Value};

use crate::{
    corpus,
    mcp::{CallContext, SessionAccess},
};

pub const WORK_READY: &str = "work_ready";
pub const WORK_DISCOVER: &str = "work_discover";
pub const WORK_CLOSE: &str = "work_close";

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
            "name": WORK_CLOSE,
            "description": "Close this session's work item because it is done. The session ends with it.",
            "inputSchema": {
                "type": "object",
                "properties": {
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
        other => Err(format!("no work tool named {other}")),
    }
}
