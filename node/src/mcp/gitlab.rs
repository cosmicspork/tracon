//! GitLab, as two narrow tools: read a merge request's state and comment on
//! it. Opening one is the review path (`review::publish`); merging, marking
//! ready, and triggering pipelines are not tools at all — "no merge" is the
//! absence of a verb, not a rule about one. The token never leaves the node.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::{broker::Broker, mcp::CallContext};

pub const CREDENTIAL: &str = "glab";
pub const MR_STATUS: &str = "mr_status";
pub const MR_COMMENT: &str = "mr_comment";

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": MR_STATUS,
            "description": "The state of a GitLab merge request: open/merged/closed, pipeline \
                            status, approvals, conflicts, and how many notes it has.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "group/project path or numeric id." },
                    "iid": { "type": "integer", "description": "The merge request's iid (the !number)." },
                },
                "required": ["project", "iid"],
            },
        }),
        json!({
            "name": MR_COMMENT,
            "description": "Post one comment on a GitLab merge request.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string" },
                    "iid": { "type": "integer" },
                    "body": { "type": "string" },
                },
                "required": ["project", "iid", "body"],
            },
        }),
    ]
}

pub async fn call(
    broker: &Arc<Broker>,
    http: &reqwest::Client,
    ctx: &CallContext,
    name: &str,
    args: &Value,
) -> Result<Value, String> {
    let env = broker
        .env_for(CREDENTIAL, &ctx.channel, &ctx.node_id)
        .map_err(|e| e.to_string())?;
    let token = env
        .get("GITLAB_TOKEN")
        .ok_or("credential glab has no GITLAB_TOKEN")?;
    let host = env
        .get("GITLAB_HOST")
        .map(|h| h.trim_end_matches('/').to_string())
        .unwrap_or_else(|| "https://gitlab.com".into());
    let host = if host.starts_with("http://") || host.starts_with("https://") {
        host
    } else {
        format!("https://{host}")
    };
    let project = args
        .get("project")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .ok_or("project is required")?;
    let iid = args
        .get("iid")
        .and_then(Value::as_i64)
        .ok_or("iid is required")?;
    let base = format!(
        "{host}/api/v4/projects/{}/merge_requests/{iid}",
        urlencode(project)
    );
    match name {
        MR_STATUS => {
            let mr = get(http, token, &base).await?;
            let approvals = get(http, token, &format!("{base}/approvals"))
                .await
                .unwrap_or(Value::Null);
            Ok(json!({
                "iid": mr["iid"],
                "title": mr["title"],
                "state": mr["state"],
                "draft": mr["draft"],
                "source_branch": mr["source_branch"],
                "target_branch": mr["target_branch"],
                "merge_status": mr["detailed_merge_status"],
                "has_conflicts": mr["has_conflicts"],
                "pipeline": mr["head_pipeline"]["status"],
                "notes": mr["user_notes_count"],
                "approved": approvals["approved"],
                "approved_by": approvals["approved_by"]
                    .as_array()
                    .map(|a| a.iter().map(|x| x["user"]["username"].clone()).collect::<Vec<_>>()),
                "web_url": mr["web_url"],
            }))
        }
        MR_COMMENT => {
            let body = args
                .get("body")
                .and_then(Value::as_str)
                .filter(|b| !b.trim().is_empty())
                .ok_or("body is required")?;
            let res = http
                .post(format!("{base}/notes"))
                .header("PRIVATE-TOKEN", token)
                .json(&json!({ "body": body }))
                .send()
                .await
                .map_err(|e| format!("gitlab: {e}"))?;
            let status = res.status();
            let v: Value = res.json().await.unwrap_or(Value::Null);
            if !status.is_success() {
                return Err(format!(
                    "gitlab refused the note ({status}): {}",
                    v["message"]
                ));
            }
            Ok(json!({ "id": v["id"], "created_at": v["created_at"] }))
        }
        other => Err(format!("no gitlab tool named {other}")),
    }
}

async fn get(http: &reqwest::Client, token: &str, url: &str) -> Result<Value, String> {
    let res = http
        .get(url)
        .header("PRIVATE-TOKEN", token)
        .send()
        .await
        .map_err(|e| format!("gitlab: {e}"))?;
    let status = res.status();
    let v: Value = res.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(format!("gitlab answered {status}: {}", v["message"]));
    }
    Ok(v)
}

/// GitLab wants `group/project` as one path segment.
fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn project_paths_are_one_segment() {
        assert_eq!(
            super::urlencode("group/sub/project"),
            "group%2Fsub%2Fproject"
        );
        assert_eq!(super::urlencode("1234"), "1234");
    }
}
