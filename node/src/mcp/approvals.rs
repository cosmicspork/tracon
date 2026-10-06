//! `approval_status`: how a caller learns what became of a call it asked the
//! operator to allow. Asking never waits; this does, briefly, the way
//! `review_status` does.

use std::time::Duration;

use serde_json::{json, Value};

use super::{
    wait::{wait_secs, MAX_WAIT_SECS},
    CallContext, SessionAccess,
};
use crate::store::approvals::{ApprovalRow, CHANGES_REQUESTED, PENDING, REJECTED, RUNNING};

pub const STATUS: &str = "approval_status";

pub fn definitions() -> Vec<Value> {
    vec![json!({
        "name": STATUS,
        "description": format!(
            "What became of calls you asked the operator to allow. A call that needs the \
             operator returns `awaiting_operator` with an `approval_id` and has NOT run; \
             when the operator allows it, the node runs it and keeps the result here. Pass \
             `approval_id`, or `approval_ids` to wait on several: the call returns as soon \
             as any of them is decided, or after up to {MAX_WAIT_SECS} seconds, with every \
             id's state — `still_waiting`, `running`, `succeeded` with the tool's result, \
             `failed` or `uncertain` with the reason, `rejected` with the operator's reason, \
             `changes_requested` with their `notes`, or `expired`. A rejected or \
             changes_requested call did not run: revise it as the operator said and call \
             the tool again, which asks anew. Call again to keep waiting."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "approval_id": { "type": "string" },
                "approval_ids": { "type": "array", "items": { "type": "string" } },
                "wait_secs": {
                    "type": "integer",
                    "description": format!(
                        "How long to block, up to {MAX_WAIT_SECS}; larger values are capped. \
                         Defaults to {MAX_WAIT_SECS}. 0 returns the current state."
                    ),
                },
            },
        },
    })]
}

pub async fn status(
    access: &SessionAccess,
    ctx: &CallContext,
    args: &Value,
) -> Result<Value, String> {
    let store = &access.store;
    let ids = ids(args)?;
    let wait = wait_secs(args);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait);
    loop {
        // Expiry is applied here too, not only on the sweeper's tick, so a
        // caller never reads an approval as waiting past its deadline.
        access.manager.expire_approvals().await;
        let mut rows = Vec::with_capacity(ids.len());
        for id in &ids {
            let row = store
                .get_approval(id)
                .map_err(|e| e.to_string())?
                .filter(|a| a.channel == ctx.channel)
                .ok_or_else(|| format!("no approval {id} on this channel"))?;
            rows.push(row);
        }
        let decided = rows
            .iter()
            .any(|a| a.state != PENDING && a.state != RUNNING);
        if decided || tokio::time::Instant::now() >= deadline {
            let views: Vec<Value> = rows.iter().map(view).collect();
            let still_waiting = !decided;
            return Ok(if args.get("approval_ids").is_some() {
                json!({ "approvals": views, "still_waiting": still_waiting })
            } else {
                views.into_iter().next().unwrap_or(Value::Null)
            });
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn ids(args: &Value) -> Result<Vec<String>, String> {
    if let Some(list) = args.get("approval_ids") {
        let ids: Vec<String> = list
            .as_array()
            .ok_or("approval_ids is a list of approval ids")?
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if ids.is_empty() {
            return Err("approval_ids names no approval".into());
        }
        return Ok(ids);
    }
    args.get("approval_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| vec![s.to_string()])
        .ok_or_else(|| "approval_id or approval_ids is required".into())
}

fn view(a: &ApprovalRow) -> Value {
    let state = if a.state == PENDING {
        "still_waiting"
    } else {
        a.state.as_str()
    };
    let parse = |text: &Option<String>| {
        text.as_deref()
            .map(|t| serde_json::from_str(t).unwrap_or_else(|_| Value::from(t)))
    };
    let mut out = json!({
        "approval_id": a.id,
        "tool": a.tool,
        "state": state,
        "result": parse(&a.result),
        "reason": a.reason,
        "notes": a.operator_note,
        // What ran, when the operator rewrote the call before allowing it.
        "edited_arguments": parse(&a.edited_arguments),
    });
    let message = match a.state.as_str() {
        CHANGES_REQUESTED => Some(format!(
            "The operator requested changes and nothing ran. Revise the call as their notes \
             say and call {} again; that asks the operator anew, with a new approval_id.",
            a.tool
        )),
        REJECTED => Some(format!(
            "The operator refused this call and nothing ran. Do not retry it as it was; if \
             their reason leaves room, revise it and call {} again.",
            a.tool
        )),
        _ => None,
    };
    if let Some(message) = message {
        out["message"] = Value::from(message);
    }
    out
}
