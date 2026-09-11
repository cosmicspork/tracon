//! GitLab, as narrow tools: read a merge request's state and comment on it,
//! read a pipeline and a job's log, and run a pipeline on a branch. Opening a
//! merge request is the review path (`review::publish`); merging and marking
//! ready are not tools at all — "no merge" is the absence of a verb, not a
//! rule about one. A pipeline on a tag, which is how a production deploy
//! runs, is refused by the tool itself. The token never leaves the node.

use serde_json::{json, Value};

use crate::{broker::SharedBroker, mcp::CallContext};

pub const CREDENTIAL: &str = "glab";
pub const MR_STATUS: &str = "mr_status";
pub const MR_COMMENT: &str = "mr_comment";
pub const PIPELINE_STATUS: &str = "pipeline_status";
pub const JOB_TRACE: &str = "job_trace";
pub const PIPELINE_RUN: &str = "pipeline_run";
pub const MR_MERGE: &str = "mr_merge";
pub const DEPLOY: &str = "deploy";

/// How much of a job's log `job_trace` returns, in KiB, unless asked.
const TRACE_KIB: u64 = 16;
const TRACE_KIB_MAX: u64 = 64;

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
        json!({
            "name": PIPELINE_STATUS,
            "description": "A pipeline's status and its jobs: by id, or the latest one for a ref.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "group/project path or numeric id." },
                    "pipeline_id": { "type": "integer" },
                    "ref": { "type": "string", "description": "A branch or tag; the latest pipeline for it." },
                },
                "required": ["project"],
            },
        }),
        json!({
            "name": JOB_TRACE,
            "description": "The end of a job's log, for reading why it failed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string" },
                    "job_id": { "type": "integer" },
                    "kib": { "type": "integer", "description": "How much of the end, in KiB (16 unless you say, at most 64)." },
                },
                "required": ["project", "job_id"],
            },
        }),
        json!({
            "name": PIPELINE_RUN,
            "description": "Run a pipeline on a branch, as the web UI's Run pipeline does. Never a \
                            tag, and never with a variable naming production: those deploys are \
                            run by hand. The operator is asked before this runs.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string" },
                    "ref": { "type": "string", "description": "The branch." },
                    "variables": { "type": "object", "description": "Pipeline variables, name to value." },
                },
                "required": ["project", "ref"],
            },
        }),
        json!({
            "name": MR_MERGE,
            "description": "Merge a GitLab merge request only when its source SHA still matches the granted revision.",
            "inputSchema": { "type": "object", "properties": {
                "project": { "type": "string" }, "iid": { "type": "integer" },
                "head_sha": { "type": "string" }, "squash": { "type": "boolean" },
                "operation_id": { "type": "string" }
            }, "required": ["project", "iid", "head_sha", "operation_id"] },
        }),
        json!({
            "name": DEPLOY,
            "description": "Play one existing manual GitLab deployment job for an immutable pipeline SHA. The pipeline and job are verified before the broker plays it.",
            "inputSchema": { "type": "object", "properties": {
                "project": { "type": "string" }, "pipeline_id": { "type": "integer" },
                "job_id": { "type": "integer" }, "environment": { "type": "string" },
                "source_sha": { "type": "string" }, "operation_id": { "type": "string" }
            }, "required": ["project", "pipeline_id", "job_id", "environment", "source_sha", "operation_id"] },
        }),
    ]
}

pub async fn call(
    broker: &SharedBroker,
    http: &reqwest::Client,
    ctx: &CallContext,
    name: &str,
    args: &Value,
    before_mutation: Option<&dyn Fn() -> Result<(), String>>,
) -> Result<Value, String> {
    let env = broker
        .read()
        .unwrap()
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
    let project_url = format!("{host}/api/v4/projects/{}", urlencode(project));
    match name {
        MR_STATUS | MR_COMMENT | MR_MERGE => {
            let iid = args
                .get("iid")
                .and_then(Value::as_i64)
                .filter(|n| *n > 0)
                .ok_or("iid is required")?;
            if name == MR_MERGE {
                let sha = args.get("head_sha").and_then(Value::as_str).filter(|v| !v.is_empty())
                    .ok_or("head_sha is required")?;
                let mr = get(http, token, &format!("{project_url}/merge_requests/{iid}")).await?;
                if mr["sha"].as_str() != Some(sha) {
                    return Err("merge request source changed since the authorized revision".into());
                }
                if let Some(recheck) = before_mutation {
                    recheck()?;
                }
                let res = http.put(format!("{project_url}/merge_requests/{iid}/merge"))
                    .header("PRIVATE-TOKEN", token)
                    .json(&json!({ "sha": sha, "squash": args["squash"].as_bool().unwrap_or(true) }))
                    .send().await.map_err(|e| format!("mutation-outcome-unknown: gitlab: {e}"))?;
                let status = res.status();
                let v: Value = res.json().await.unwrap_or(Value::Null);
                if !status.is_success() {
                    let error = format!("gitlab refused the merge ({status}): {}", v["message"]);
                    return Err(if status.is_server_error() {
                        format!("mutation-outcome-unknown: {error}")
                    } else { error });
                }
                Ok(json!({ "state": v["state"], "merge_commit_sha": v["merge_commit_sha"], "web_url": v["web_url"] }))
            } else {
                merge_request(http, token, &project_url, iid, name, args).await
            }
        }
        PIPELINE_STATUS => {
            let id = match (
                args.get("pipeline_id").and_then(Value::as_i64),
                args.get("ref").and_then(Value::as_str),
            ) {
                (Some(id), _) => id,
                (None, Some(r)) => {
                    let latest = get(
                        http,
                        token,
                        &format!("{project_url}/pipelines?ref={}&per_page=1", urlencode(r)),
                    )
                    .await?;
                    latest[0]["id"]
                        .as_i64()
                        .ok_or_else(|| format!("no pipeline has run for {r}"))?
                }
                (None, None) => return Err("pipeline_id or ref is required".into()),
            };
            let p = get(http, token, &format!("{project_url}/pipelines/{id}")).await?;
            let jobs = get(
                http,
                token,
                &format!("{project_url}/pipelines/{id}/jobs?per_page=100"),
            )
            .await
            .unwrap_or(Value::Null);
            let jobs: Vec<Value> = jobs
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|j| {
                            json!({ "id": j["id"], "name": j["name"], "stage": j["stage"], "status": j["status"] })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(json!({
                "id": p["id"],
                "ref": p["ref"],
                "sha": p["sha"],
                "status": p["status"],
                "source": p["source"],
                "web_url": p["web_url"],
                "jobs": jobs,
            }))
        }
        JOB_TRACE => {
            let job = args
                .get("job_id")
                .and_then(Value::as_i64)
                .ok_or("job_id is required")?;
            let kib = args
                .get("kib")
                .and_then(Value::as_u64)
                .unwrap_or(TRACE_KIB)
                .clamp(1, TRACE_KIB_MAX);
            let res = http
                .get(format!("{project_url}/jobs/{job}/trace"))
                .header("PRIVATE-TOKEN", token)
                .send()
                .await
                .map_err(|e| format!("gitlab: {e}"))?;
            let status = res.status();
            let log = res.text().await.unwrap_or_default();
            if !status.is_success() {
                return Err(format!("gitlab answered {status} for job {job}'s log"));
            }
            let (tail, truncated) = tail(&log, (kib * 1024) as usize);
            Ok(json!({ "job_id": job, "truncated": truncated, "log": tail }))
        }
        PIPELINE_RUN => {
            let r = args
                .get("ref")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .ok_or("ref is required (a branch)")?;
            let variables = pipeline_variables(args.get("variables"))?;
            let tag = http
                .get(format!("{project_url}/repository/tags/{}", urlencode(r)))
                .header("PRIVATE-TOKEN", token)
                .send()
                .await
                .map_err(|e| format!("gitlab: {e}"))?;
            if tag.status().is_success() {
                return Err(format!(
                    "{r} is a tag; a pipeline on a tag is a release, and that is run by hand"
                ));
            }
            let res = http
                .post(format!("{project_url}/pipeline"))
                .header("PRIVATE-TOKEN", token)
                .json(&json!({ "ref": r, "variables": variables }))
                .send()
                .await
                .map_err(|e| format!("gitlab: {e}"))?;
            let status = res.status();
            let v: Value = res.json().await.unwrap_or(Value::Null);
            if !status.is_success() {
                return Err(format!(
                    "gitlab refused the pipeline ({status}): {}",
                    v["message"]
                ));
            }
            Ok(json!({ "id": v["id"], "status": v["status"], "web_url": v["web_url"] }))
        }
        DEPLOY => {
            let pipeline_id = args.get("pipeline_id").and_then(Value::as_i64).filter(|id| *id > 0)
                .ok_or("pipeline_id is required")?;
            let job_id = args.get("job_id").and_then(Value::as_i64).filter(|id| *id > 0)
                .ok_or("job_id is required")?;
            let environment = args.get("environment").and_then(Value::as_str).map(str::trim)
                .filter(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
                .ok_or("environment is required")?;
            let source_sha = args.get("source_sha").and_then(Value::as_str).filter(|v| v.len() >= 7)
                .ok_or("source_sha is required")?;
            let pipeline = get(http, token, &format!("{project_url}/pipelines/{pipeline_id}")).await?;
            if pipeline["sha"].as_str() != Some(source_sha) {
                return Err("pipeline does not contain the authorized source SHA".into());
            }
            let job = get(http, token, &format!("{project_url}/jobs/{job_id}")).await?;
            if job["pipeline"]["id"].as_i64() != Some(pipeline_id) {
                return Err("job does not belong to the identified pipeline".into());
            }
            if job["status"].as_str() != Some("manual") {
                return Err("deployment job is not waiting for a manual play".into());
            }
            if job["environment"]["name"].as_str() != Some(environment) {
                return Err("deployment job does not target the authorized environment".into());
            }
            if let Some(recheck) = before_mutation {
                recheck()?;
            }
            let res = http.post(format!("{project_url}/jobs/{job_id}/play")).header("PRIVATE-TOKEN", token)
                .json(&json!({})).send().await.map_err(|e| format!("mutation-outcome-unknown: gitlab: {e}"))?;
            let status = res.status();
            let v: Value = res.json().await.unwrap_or(Value::Null);
            if !status.is_success() {
                let error = format!("gitlab refused the deployment job ({status}): {}", v["message"]);
                return Err(if status.is_server_error() {
                    format!("mutation-outcome-unknown: {error}")
                } else { error });
            }
            Ok(json!({ "id": v["id"], "status": v["status"], "web_url": v["web_url"], "environment": environment, "source_sha": source_sha, "pipeline_id": pipeline_id }))
        }
        other => Err(format!("no gitlab tool named {other}")),
    }
}

async fn merge_request(
    http: &reqwest::Client,
    token: &str,
    project_url: &str,
    iid: i64,
    name: &str,
    args: &Value,
) -> Result<Value, String> {
    let base = format!("{project_url}/merge_requests/{iid}");
    if name == MR_STATUS {
        let mr = get(http, token, &base).await?;
        let approvals = get(http, token, &format!("{base}/approvals"))
            .await
            .unwrap_or(Value::Null);
        return Ok(json!({
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
        }));
    }
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

/// Pipeline variables as GitLab takes them. A value naming production is
/// refused here, before anything is sent: that is the deploy that is run by
/// hand, whatever the ref.
fn pipeline_variables(v: Option<&Value>) -> Result<Vec<Value>, String> {
    let Some(v) = v.filter(|v| !v.is_null()) else {
        return Ok(Vec::new());
    };
    let map = v
        .as_object()
        .ok_or("variables is an object of name to value")?;
    map.iter()
        .map(|(k, v)| {
            let value = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            if value.to_ascii_lowercase().contains("production") {
                return Err(format!(
                    "variable {k} names production; a production deploy is run by hand"
                ));
            }
            Ok(json!({ "key": k, "value": value }))
        })
        .collect()
}

/// The last `max` bytes of a log, cut on a character boundary.
fn tail(log: &str, max: usize) -> (&str, bool) {
    if log.len() <= max {
        return (log, false);
    }
    let mut start = log.len() - max;
    while !log.is_char_boundary(start) {
        start += 1;
    }
    (&log[start..], true)
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
    use super::*;

    #[test]
    fn project_paths_are_one_segment() {
        assert_eq!(urlencode("group/sub/project"), "group%2Fsub%2Fproject");
        assert_eq!(urlencode("1234"), "1234");
    }

    #[test]
    fn a_log_tail_is_cut_on_a_character_boundary() {
        assert_eq!(tail("short", 100), ("short", false));
        let (t, cut) = tail("aaaé", 2);
        assert!(cut);
        assert_eq!(t, "é");
    }

    #[test]
    fn a_production_variable_is_refused_by_name() {
        let e = pipeline_variables(Some(&json!({ "ENVIRONMENT": "Production" }))).unwrap_err();
        assert!(e.contains("ENVIRONMENT"), "{e}");
        let ok = pipeline_variables(Some(&json!({ "DEPLOY": "staging", "N": 2 }))).unwrap();
        assert_eq!(ok.len(), 2);
        assert!(pipeline_variables(None).unwrap().is_empty());
    }
}
