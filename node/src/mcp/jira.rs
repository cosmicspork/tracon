//! Jira, as two narrow tools: read an issue and comment on it. Transitions
//! are not a tool: moving a ticket's status desyncs what the operator is
//! actually working on, so the verb does not exist here. The API token
//! never leaves the node.

use serde_json::{json, Value};

use crate::{broker::SharedBroker, mcp::CallContext};

pub const CREDENTIAL: &str = "jira";
pub const ISSUE: &str = "issue";
pub const ISSUE_COMMENT: &str = "issue_comment";

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": ISSUE,
            "description": "A Jira issue: summary, status, assignee, description, and its most \
                            recent comments.",
            "inputSchema": {
                "type": "object",
                "properties": { "key": { "type": "string", "description": "Issue key, e.g. WRK-123." } },
                "required": ["key"],
            },
        }),
        json!({
            "name": ISSUE_COMMENT,
            "description": "Post one comment on a Jira issue. Status changes are the operator's; \
                            say what you would transition and why instead.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string" },
                    "body": { "type": "string" },
                },
                "required": ["key", "body"],
            },
        }),
    ]
}

pub async fn call(
    broker: &SharedBroker,
    http: &reqwest::Client,
    ctx: &CallContext,
    name: &str,
    args: &Value,
) -> Result<Value, String> {
    let env = broker
        .read()
        .unwrap()
        .env_for(CREDENTIAL, &ctx.channel, &ctx.node_id)
        .map_err(|e| e.to_string())?;
    let url = env
        .get("JIRA_URL")
        .map(|u| u.trim_end_matches('/').to_string())
        .ok_or("credential jira has no JIRA_URL")?;
    let email = env
        .get("JIRA_EMAIL")
        .ok_or("credential jira has no JIRA_EMAIL")?;
    let token = env
        .get("JIRA_TOKEN")
        .ok_or("credential jira has no JIRA_TOKEN")?;
    let key = args
        .get("key")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|k| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        .ok_or("key is required (e.g. WRK-123)")?;
    let base = format!("{url}/rest/api/2/issue/{key}");
    match name {
        ISSUE => {
            let res = http
                .get(format!(
                    "{base}?fields=summary,status,assignee,description,comment,issuetype,priority"
                ))
                .basic_auth(email, Some(token))
                .send()
                .await
                .map_err(|e| format!("jira: {e}"))?;
            let status = res.status();
            let v: Value = res.json().await.unwrap_or(Value::Null);
            if !status.is_success() {
                return Err(format!("jira answered {status}: {}", v["errorMessages"]));
            }
            let f = &v["fields"];
            let comments: Vec<Value> = f["comment"]["comments"]
                .as_array()
                .map(|c| {
                    c.iter()
                        .rev()
                        .take(10)
                        .map(|c| {
                            json!({
                                "author": c["author"]["displayName"],
                                "created": c["created"],
                                "body": c["body"],
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(json!({
                "key": v["key"],
                "type": f["issuetype"]["name"],
                "summary": f["summary"],
                "status": f["status"]["name"],
                "priority": f["priority"]["name"],
                "assignee": f["assignee"]["displayName"],
                "description": f["description"],
                "comments": comments,
            }))
        }
        ISSUE_COMMENT => {
            let body = args
                .get("body")
                .and_then(Value::as_str)
                .filter(|b| !b.trim().is_empty())
                .ok_or("body is required")?;
            let res = http
                .post(format!("{base}/comment"))
                .basic_auth(email, Some(token))
                .json(&json!({ "body": body }))
                .send()
                .await
                .map_err(|e| format!("jira: {e}"))?;
            let status = res.status();
            let v: Value = res.json().await.unwrap_or(Value::Null);
            if !status.is_success() {
                return Err(format!(
                    "jira refused the comment ({status}): {}",
                    v["errorMessages"]
                ));
            }
            Ok(json!({ "id": v["id"], "created": v["created"] }))
        }
        other => Err(format!("no jira tool named {other}")),
    }
}
