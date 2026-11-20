//! `approval_status`: how a caller learns what became of a call it asked the
//! operator to allow. Asking never waits; this does, briefly, the way
//! `review_status` does.

use std::time::Duration;

use serde_json::{json, Value};

use super::{wait::wait_secs, CallContext, SessionAccess};
use crate::store::approvals::{
    ApprovalRow, CHANGES_REQUESTED, FAILED, PENDING, REJECTED, RUNNING, STALE_REASON,
};

pub const STATUS: &str = "approval_status";

pub fn definitions() -> Vec<Value> {
    vec![json!({
        "name": STATUS,
        "description": format!(
            "What became of calls you asked the operator to allow. A call that needs the \
             operator returns `awaiting_operator` with an `approval_id` and has NOT run; \
             when the operator allows it, the node runs it and keeps the result here. Pass \
             `approval_id`, or `approval_ids` to wait on several: the call returns as soon \
             as any of them is decided, with every id's state — `still_waiting`, `running`, \
             `succeeded` with the tool's result, `failed` or `uncertain` with the reason \
             (`failed` with `stale: true` when what the operator saw had moved before it \
             ran: nothing was sent, so call the tool again for a fresh card), `rejected` with the operator's reason, `changes_requested` with their `notes`, \
             or `expired`. A rejected or changes_requested call did not run: revise it as \
             the operator said and call the tool again, which asks anew. {}",
            super::wait::pointer()
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "approval_id": { "type": "string" },
                "approval_ids": { "type": "array", "items": { "type": "string" } },
                "wait_secs": {
                    "type": "integer",
                    "description": super::wait::wait_secs_description(),
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
    let wait = wait_secs(ctx, args);
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
            if let Some(session_id) = ctx.session_id() {
                for row in rows.iter().filter(|a| a.is_settled()) {
                    access
                        .manager
                        .waits()
                        .tell(session_id, &crate::session::waits::approval_key(row));
                }
            }
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

/// Failed because the plugin found what the card showed had moved.
fn is_stale(a: &ApprovalRow) -> bool {
    a.reason
        .as_deref()
        .is_some_and(|r| r.starts_with(STALE_REASON))
}

/// What `approval_status` says of one approval.
pub(crate) fn view(a: &ApprovalRow) -> Value {
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
    let card = a.tool == crate::store::approvals::PLUGIN_CARD_TOOL;
    let plugin = || {
        serde_json::from_str::<Value>(&a.arguments)
            .ok()
            .and_then(|args| args["plugin"].as_str().map(str::to_string))
            .unwrap_or_else(|| "the plugin".into())
    };
    let message = match a.state.as_str() {
        // A plugin's card for a call of yours: the plugin acts on the
        // decision, and its own tool says what came of it.
        CHANGES_REQUESTED | REJECTED if card => Some(format!(
            "The operator did not approve what plugin {} asked on your behalf, and it did not \
             act. Revise the call to its tool as their {} say and call it again.",
            plugin(),
            if a.state == CHANGES_REQUESTED {
                "notes"
            } else {
                "reason"
            }
        )),
        crate::store::approvals::SUCCEEDED if card => Some(format!(
            "The operator approved what plugin {} asked on your behalf; the plugin now acts on \
             it. Its own tools say what came of it.",
            plugin()
        )),
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
        FAILED if is_stale(a) => {
            out["stale"] = Value::from(true);
            Some(format!(
                "What the operator saw changed before this call ran, and the plugin sent \
                 nothing. Call {} again for a fresh card.",
                a.tool
            ))
        }
        _ => None,
    };
    if let Some(message) = message {
        out["message"] = Value::from(message);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(reason: &str) -> ApprovalRow {
        serde_json::from_value(json!({
            "id": "a1", "channel": "personal", "session_id": null, "node_id": "n1",
            "lane": null, "tool": "fake__query", "arguments": "{}", "request_key": "k",
            "title": "t", "state": FAILED, "answer_option_id": "allow_once",
            "edited_arguments": null, "result": null, "reason": reason,
            "operator_note": null, "claimed_ms": null, "created_ms": 1, "decided_ms": 2,
            "finished_ms": 3, "expires_ms": 4,
        }))
        .unwrap()
    }

    #[test]
    fn a_call_failed_as_stale_says_to_call_again() {
        let stale = view(&failed(
            "stale: plugin fake: the issue moved. Call it again.",
        ));
        assert_eq!(stale["state"], "failed");
        assert_eq!(stale["stale"], true);
        assert!(stale["message"]
            .as_str()
            .unwrap()
            .contains("Call fake__query again"));
        let other = view(&failed("plugin fake: no"));
        assert!(other.get("stale").is_none());
        assert!(other.get("message").is_none());
    }
}
