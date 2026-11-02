//! What a session's brokered calls were, and who decided each: the policy,
//! a grant, the node's own rules, or the operator (and from where).
//!
//! Every call a session makes to the node's tools records one `tool_decided`
//! event as it returns. A call held for the operator names its approval, and
//! what became of it is read back from the approval and from the events the
//! answer recorded, so the view is assembled from what the node already
//! keeps rather than a second ledger that could disagree with it.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};

use crate::session::state::event_kind as ek;
use crate::store::approvals::{self as a, ApprovalRow};
use crate::store::{EventRow, Store};

/// How a call was decided, as the audit names it.
pub const POLICY_ALLOWED: &str = "policy_allowed";
/// Run without the policy: the node's own tools that ask nothing of a
/// credential (asking the operator, reading an approval) and the phase's own
/// artifact.
pub const NODE_ALLOWED: &str = "node_allowed";
pub const DENIED: &str = "denied";
/// Refused by the node before any policy or operator saw it.
pub const REFUSED: &str = "refused";
/// Held for the operator; the approval says what became of it.
pub const ASKED: &str = "asked";
pub const PENDING: &str = "pending";
pub const APPROVED: &str = "approved";
/// Allowed with the operator's rewrite of the arguments.
pub const EDITED: &str = "edited";
pub const REJECTED: &str = "rejected";
pub const CHANGES_REQUESTED: &str = "changes_requested";
pub const EXPIRED: &str = "expired";

/// Longest text an audit record keeps of one argument.
const MAX_TEXT: usize = 2_000;

/// Argument names whose values are credentials, whatever they hold.
const SECRET_NAMES: &[&str] = &[
    "token",
    "password",
    "passwd",
    "secret",
    "api_key",
    "apikey",
    "authorization",
    "cookie",
    "credential",
    "private_key",
];

fn secret_name(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    SECRET_NAMES.iter().any(|name| key.contains(name))
}

/// `arguments` as the audit keeps and shows them: a value under a
/// credential's name replaced, every line that looks like a secret replaced
/// as a drafted report's are, and long text cut short.
pub fn redacted(arguments: &Value) -> Value {
    match arguments {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    let shown = if secret_name(k) && !v.is_null() {
                        Value::from(crate::config::edit::REDACTED)
                    } else {
                        redacted(v)
                    };
                    (k.clone(), shown)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redacted).collect()),
        Value::String(text) => Value::String(bounded(&super::operator::scrub(text))),
        other => other.clone(),
    }
}

fn bounded(text: &str) -> String {
    let count = text.chars().count();
    if count <= MAX_TEXT {
        return text.to_string();
    }
    let kept: String = text.chars().take(MAX_TEXT).collect();
    format!("{kept}… ({} more characters)", count - MAX_TEXT)
}

/// How one call through [`super::Tools::call`] was decided. Filled in as the
/// call is gated; whatever is left at its default is the node refusing it.
#[derive(Debug, Clone)]
pub struct Gate {
    pub decision: &'static str,
    /// `policy`, `grant` or `node`. The operator decides only approvals,
    /// which name themselves.
    pub decided_by: &'static str,
    pub rule: Option<String>,
    pub reason: Option<String>,
    pub approval_id: Option<String>,
}

impl Default for Gate {
    fn default() -> Self {
        Self {
            decision: REFUSED,
            decided_by: "node",
            rule: None,
            reason: None,
            approval_id: None,
        }
    }
}

impl Gate {
    pub fn node(rule: &str) -> Self {
        Self {
            decision: NODE_ALLOWED,
            rule: Some(rule.to_string()),
            ..Default::default()
        }
    }

    pub fn asked(result: &Result<Value, String>, by: &'static str, rule: Option<String>) -> Self {
        Self {
            decision: ASKED,
            decided_by: by,
            rule,
            approval_id: approval_of(result),
            ..Default::default()
        }
    }
}

/// The approval a held call's answer names.
pub fn approval_of(result: &Result<Value, String>) -> Option<String> {
    result
        .as_ref()
        .ok()
        .and_then(|v| v.get("approval_id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The `tool_decided` payload for one call.
pub fn record(name: &str, args: &Value, gate: &Gate, result: &Result<Value, String>) -> Value {
    let (outcome, error) = match result {
        Ok(_) if gate.decision == ASKED => ("held", None),
        Ok(_) => ("ok", None),
        Err(e) => ("error", Some(bounded(e))),
    };
    let mut title = super::summarize(name, &redacted(args));
    if title.chars().count() > 200 {
        title = title.chars().take(200).collect::<String>() + "…";
    }
    json!({
        "tool": name,
        "title": title,
        "arguments": redacted(args),
        "decision": gate.decision,
        "decided_by": gate.decided_by,
        "rule": gate.rule,
        "reason": gate.reason,
        "approval_id": gate.approval_id,
        "outcome": outcome,
        "error": error,
    })
}

const KINDS: &[&str] = &[
    ek::TOOL_DECIDED,
    ek::PERMISSION_REQUEST,
    ek::PERMISSION_ANSWER,
    ek::PERMISSION_EXPIRED,
    ek::APPROVAL_SETTLED,
];

/// What the events say of one approval, for when this node does not hold its
/// row (a peer's session, mirrored here).
#[derive(Default)]
struct Told {
    answer: Option<EventRow>,
    expired_ms: Option<i64>,
    settled: Option<(String, Option<String>)>,
}

/// Every brokered call `session_id` made, oldest first, each with how it was
/// decided and by whom. Arguments are redacted as [`redacted`] says.
pub fn session_audit(
    store: &Store,
    session_id: &str,
) -> Result<Vec<Value>, crate::store::StoreError> {
    let events = store.session_events_of_kinds(session_id, KINDS)?;
    let approval_id = |e: &EventRow| {
        e.payload
            .get("approval_id")
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let mut told: HashMap<String, Told> = HashMap::new();
    let mut requests: Vec<(String, &EventRow)> = Vec::new();
    for e in &events {
        let Some(id) = approval_id(e) else { continue };
        let entry = told.entry(id.clone()).or_default();
        match e.kind.as_str() {
            ek::PERMISSION_REQUEST => requests.push((id, e)),
            ek::PERMISSION_ANSWER => entry.answer = Some(e.clone()),
            ek::PERMISSION_EXPIRED => entry.expired_ms = Some(e.at_ms),
            ek::APPROVAL_SETTLED => {
                entry.settled = Some((
                    e.payload["state"].as_str().unwrap_or_default().to_string(),
                    e.payload["reason"].as_str().map(str::to_string),
                ))
            }
            _ => {}
        }
    }
    let mut rows: HashMap<String, Option<ApprovalRow>> = HashMap::new();
    let mut row_of = |id: &str| -> Result<Option<ApprovalRow>, crate::store::StoreError> {
        if let Some(row) = rows.get(id) {
            return Ok(row.clone());
        }
        let row = store.get_approval(id)?;
        rows.insert(id.to_string(), row.clone());
        Ok(row)
    };

    let mut out = Vec::new();
    let mut covered = HashSet::new();
    for e in events.iter().filter(|e| e.kind == ek::TOOL_DECIDED) {
        let mut entry = e.payload.clone();
        entry["at_ms"] = json!(e.at_ms);
        if let Some(id) = approval_id(e) {
            covered.insert(id.clone());
            let row = row_of(&id)?;
            resolve(&mut entry, row.as_ref(), told.get(&id));
        }
        out.push(entry);
    }
    // Approvals no call of this session raised through the tools: a refused
    // connection's egress ask, or a call from before calls were recorded.
    for (id, e) in requests {
        if !covered.insert(id.clone()) {
            continue;
        }
        let row = row_of(&id)?;
        let arguments = row
            .as_ref()
            .and_then(|r| serde_json::from_str::<Value>(&r.arguments).ok())
            .unwrap_or_else(|| e.payload["raw_input"]["arguments"].clone());
        let tool = e.payload["tool"].as_str().unwrap_or_default();
        let mut entry = json!({
            "at_ms": e.at_ms,
            "tool": tool,
            "title": e.payload["title"],
            "arguments": redacted(&arguments),
            "decision": ASKED,
            "decided_by": "node",
            "rule": null,
            "reason": null,
            "approval_id": id,
            "outcome": "held",
            "error": null,
        });
        resolve(&mut entry, row.as_ref(), told.get(&id));
        out.push(entry);
    }
    out.sort_by_key(|e| e["at_ms"].as_i64().unwrap_or_default());
    Ok(out)
}

/// Fill in what became of a held call: the operator's decision, who made it
/// and when, and how the run went. The approval's row is the record when
/// this node holds it; the answer's event says who answered.
fn resolve(entry: &mut Value, row: Option<&ApprovalRow>, told: Option<&Told>) {
    let answer = told.and_then(|t| t.answer.as_ref());
    let by = answer
        .and_then(|e| e.payload["decided_by"].as_str())
        .unwrap_or("operator")
        .to_string();
    if let Some(row) = row {
        let edited = row.edited_arguments.as_deref().and_then(|edit| {
            let edit = serde_json::from_str::<Value>(edit).ok()?;
            let asked = serde_json::from_str::<Value>(&row.arguments).ok();
            (asked.as_ref() != Some(&edit)).then_some(edit)
        });
        let (decision, decided_by) = match row.state.as_str() {
            a::PENDING => (PENDING, None),
            a::REJECTED => (REJECTED, Some(by)),
            a::CHANGES_REQUESTED => (CHANGES_REQUESTED, Some(by)),
            a::EXPIRED => (EXPIRED, Some("nobody".to_string())),
            _ if edited.is_some() => (EDITED, Some(by)),
            _ => (APPROVED, Some(by)),
        };
        entry["decision"] = json!(decision);
        entry["decided_by"] = json!(decided_by);
        entry["decided_ms"] = json!(row
            .decided_ms
            .or(row.finished_ms.filter(|_| decision == EXPIRED)));
        entry["approval_state"] = json!(row.state);
        if let Some(edit) = edited {
            entry["edited_arguments"] = redacted(&edit);
        }
        if matches!(row.state.as_str(), a::FAILED | a::UNCERTAIN | a::REJECTED) {
            entry["reason"] = json!(row.reason);
        }
        if let Some(note) = &row.operator_note {
            entry["note"] = json!(note);
        }
        return;
    }
    // A peer's session: only its events are here.
    let Some(told) = told else { return };
    if let Some(answer) = &told.answer {
        let state = answer.payload["state"].as_str().unwrap_or_default();
        let decision = match state {
            a::REJECTED => REJECTED,
            a::CHANGES_REQUESTED => CHANGES_REQUESTED,
            _ if answer.payload["edited"] == true => EDITED,
            _ => APPROVED,
        };
        entry["decision"] = json!(decision);
        entry["decided_by"] = json!(by);
        entry["decided_ms"] = json!(answer.at_ms);
        if decision == EDITED {
            entry["edited_arguments"] = answer.payload["edited_arguments"].clone();
        }
        let settled = told.settled.as_ref().map(|(s, _)| s.as_str());
        entry["approval_state"] = json!(settled.unwrap_or(state));
    } else if let Some(at) = told.expired_ms {
        entry["decision"] = json!(EXPIRED);
        entry["decided_by"] = json!("nobody");
        entry["decided_ms"] = json!(at);
        entry["approval_state"] = json!(a::EXPIRED);
    } else {
        entry["decision"] = json!(PENDING);
        entry["decided_by"] = Value::Null;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credential_by_name_or_by_look_is_not_kept() {
        let shown = redacted(&json!({
            "slug": "note-x",
            "api_token": "abc",
            "headers": { "Authorization": "Bearer zzz" },
            "body": "line one\npassword=hunter2\nline three",
            "items": ["ghp_0123456789abcdef", "fine"],
            "count": 3,
        }));
        assert_eq!(shown["slug"], "note-x");
        assert_eq!(shown["api_token"], "[redacted]");
        assert_eq!(shown["headers"]["Authorization"], "[redacted]");
        assert_eq!(
            shown["body"],
            "line one\n[redacted secret-looking content]\nline three"
        );
        assert_eq!(shown["items"][0], "[redacted secret-looking content]");
        assert_eq!(shown["items"][1], "fine");
        assert_eq!(shown["count"], 3);
    }

    #[test]
    fn long_text_is_cut_short_and_says_by_how_much() {
        let shown = redacted(&json!({ "body": "x".repeat(MAX_TEXT + 5) }));
        let body = shown["body"].as_str().unwrap();
        assert!(body.ends_with("… (5 more characters)"), "{body}");
    }
}
