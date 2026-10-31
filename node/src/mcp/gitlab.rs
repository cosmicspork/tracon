//! GitLab, as narrow tools: read a merge request's state and its discussions,
//! comment on it or reply to one discussion, mark it ready or back to draft,
//! find the open one for a branch,
//! read a pipeline and a job's log, list the pipelines that ran at an exact
//! commit and play a manual job in one, run a pipeline on a branch, and
//! retry a pipeline's failed jobs (`super::parity` maps each to GitHub's).
//! Opening a merge request is the review path (`review::publish`); merging
//! (`mr_merge`) and playing a deploy job (`deploy`) run only with current
//! scoped authority. A pipeline on a tag, which is how a
//! production deploy runs, is refused by the tool itself. GitLab's own
//! pipeline-creation API resolves `ref` only against a branch or tag, never
//! a bare commit SHA, so an exact-SHA deploy never creates a pipeline: it
//! finds one GitLab already ran at that SHA and plays its configured manual
//! job. The token never leaves the node.

use serde_json::{json, Value};

use crate::{broker::SharedBroker, mcp::CallContext};

pub const CREDENTIAL: &str = "glab";
pub const MR_STATUS: &str = "mr_status";
pub const MR_COMMENT: &str = "mr_comment";
pub const PIPELINE_STATUS: &str = "pipeline_status";
pub const PIPELINE_WAIT: &str = "pipeline_wait";
pub const PIPELINE_LIST_BY_SHA: &str = "pipeline_list_by_sha";
pub const JOB_TRACE: &str = "job_trace";
pub const JOB_PLAY: &str = "job_play";
pub const PIPELINE_RUN: &str = "pipeline_run";
pub const MR_MERGE: &str = "mr_merge";
pub const MR_READY: &str = "mr_ready";
pub const DEPLOY: &str = "deploy";
pub const MR_DISCUSSIONS: &str = "mr_discussions";
pub const MR_REPLY: &str = "mr_reply";
pub const MR_FOR_BRANCH: &str = "mr_for_branch";
pub const PIPELINE_RETRY: &str = "pipeline_retry";

/// Every tool here, each of which takes the repository as `project`.
pub const TOOLS: &[&str] = &[
    MR_STATUS,
    MR_COMMENT,
    PIPELINE_STATUS,
    PIPELINE_WAIT,
    PIPELINE_LIST_BY_SHA,
    JOB_TRACE,
    JOB_PLAY,
    PIPELINE_RUN,
    MR_MERGE,
    MR_READY,
    DEPLOY,
    MR_DISCUSSIONS,
    MR_REPLY,
    MR_FOR_BRANCH,
    PIPELINE_RETRY,
];

/// What `project` is. Optional in what a session on a GitLab repository is
/// offered (`Tools::list_offered`), required everywhere else.
const PROJECT: &str = "group/project path or numeric id. In a session on a GitLab \
                       repository, defaults to that project; required outside a session.";

/// One page of discussions: a review is read in one call, and a merge request
/// with more than this says so rather than pretending it has none.
const DISCUSSIONS_MAX: usize = 100;

/// How much of a job's log `job_trace` returns, in KiB, unless asked.
const TRACE_KIB: u64 = 16;
const TRACE_KIB_MAX: u64 = 64;
/// `pipeline_list_by_sha` never hands back more than this many candidates:
/// an exact-SHA deploy inspects a handful of pipelines, not a project's
/// whole history.
const SHA_PIPELINES_MAX: u32 = 20;

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": MR_STATUS,
            "description": "The state of a GitLab merge request: open/merged/closed, pipeline \
                            status, approvals, conflicts, and how many notes it has.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": PROJECT },
                    "iid": { "type": "integer", "description": "The merge request's iid (the !number)." },
                },
                "required": ["project", "iid"],
            },
        }),
        json!({
            "name": MR_COMMENT,
            "description": "Post one comment on a GitLab merge request. The operator is asked \
                            before it posts. An asked call returns `awaiting_operator` with an `approval_id` at once, and approval_status reports the outcome.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": PROJECT },
                    "iid": { "type": "integer" },
                    "body": { "type": "string" },
                },
                "required": ["project", "iid", "body"],
            },
        }),
        json!({
            "name": MR_DISCUSSIONS,
            "description": "A GitLab merge request's review feedback: its diff discussions (id, \
                            resolved, file and line, each note) and its general comments, \
                            without system notes. Pass a discussion's id to mr_reply.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": PROJECT },
                    "iid": { "type": "integer" },
                    "unresolved_only": { "type": "boolean", "description": "Leave out resolved discussions." },
                },
                "required": ["project", "iid"],
            },
        }),
        json!({
            "name": MR_REPLY,
            "description": "Reply to one discussion on a GitLab merge request, and optionally \
                            resolve it. The operator is asked before it posts. An asked call returns `awaiting_operator` with an `approval_id` at once, and approval_status reports the outcome.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": PROJECT },
                    "iid": { "type": "integer" },
                    "discussion_id": { "type": "string", "description": "The discussion's id from mr_discussions." },
                    "body": { "type": "string", "description": "The reply. May be omitted when only resolving." },
                    "resolve": { "type": "boolean", "description": "Mark the discussion resolved after replying." },
                },
                "required": ["project", "iid", "discussion_id"],
            },
        }),
        json!({
            "name": MR_FOR_BRANCH,
            "description": "The open GitLab merge request for a source branch, if there is one: \
                            its iid is what submit_review takes as `change`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": PROJECT },
                    "branch": { "type": "string" },
                },
                "required": ["project", "branch"],
            },
        }),
        json!({
            "name": PIPELINE_STATUS,
            "description": "A pipeline's status and its jobs: by id, or the latest one for a ref.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": PROJECT },
                    "pipeline_id": { "type": "integer" },
                    "ref": { "type": "string", "description": "A branch or tag; the latest pipeline for it." },
                },
                "required": ["project"],
            },
        }),
        json!({
            "name": PIPELINE_WAIT,
            "description": format!(
                "Wait for a pipeline to move: returns as soon as it or one of its jobs changes \
                 state, when it has finished, or after wait_secs, with the same status and jobs \
                 as pipeline_status and a `state` token. Pass that token back as `since` so a \
                 change between calls is not missed. `until: \"finished\"` holds through job \
                 moves until the pipeline finishes, and `until: \"failure\"` until then or a job \
                 fails (one allowed to fail does not count). A failed job's log is job_trace. {}",
                super::wait::pointer()
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": PROJECT },
                    "pipeline_id": { "type": "integer" },
                    "since": { "type": "string", "description": "The `state` an earlier call returned." },
                    "until": super::wait::until_schema(),
                    "wait_secs": { "type": "integer", "description": "How long to hold, at most 45 (the default)." },
                },
                "required": ["project", "pipeline_id"],
            },
        }),
        json!({
            "name": PIPELINE_LIST_BY_SHA,
            "description": "Pipelines GitLab already ran at an exact commit SHA (never resolved \
                            against a branch or tag): id, ref, status, and web URL, newest first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": PROJECT },
                    "sha": { "type": "string", "description": "The exact commit SHA." },
                },
                "required": ["project", "sha"],
            },
        }),
        json!({
            "name": JOB_TRACE,
            "description": "The end of a job's log, for reading why it failed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": PROJECT },
                    "job_id": { "type": "integer" },
                    "kib": { "type": "integer", "description": "How much of the end, in KiB (16 unless you say, at most 64)." },
                },
                "required": ["project", "job_id"],
            },
        }),
        json!({
            "name": JOB_PLAY,
            "description": "Play a manual job, as the web UI's Play button does. Never with a \
                            variable naming production: those deploys are run by hand. The \
                            operator is asked before this runs. An asked call returns `awaiting_operator` with an `approval_id` at once, and approval_status reports the outcome.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": PROJECT },
                    "job_id": { "type": "integer" },
                    "variables": { "type": "object", "description": "Job variables, name to string value. Values are sent as given; a number or boolean is refused, so quote it." },
                },
                "required": ["project", "job_id"],
            },
        }),
        json!({
            "name": PIPELINE_RUN,
            "description": "Run a pipeline on a branch, as the web UI's Run pipeline does. Never a \
                            tag, and never with a variable naming production: those deploys are \
                            run by hand. The operator is asked before this runs. An asked call returns `awaiting_operator` with an `approval_id` at once, and approval_status reports the outcome.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": PROJECT },
                    "ref": { "type": "string", "description": "The branch." },
                    "variables": { "type": "object", "description": "Pipeline variables, name to string value. Values are sent as given; a number or boolean is refused, so quote it." },
                },
                "required": ["project", "ref"],
            },
        }),
        json!({
            "name": PIPELINE_RETRY,
            "description": "Retry a pipeline's failed and canceled jobs, as the web UI's Retry \
                            does. Never a pipeline on a tag, or one whose variables name \
                            production: those deploys are run by hand. The operator is asked \
                            before this runs. An asked call returns `awaiting_operator` with an `approval_id` at once, and approval_status reports the outcome. Once \
                            it ran, the node follows the pipeline and pipeline_wait waits for it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": PROJECT },
                    "pipeline_id": { "type": "integer" },
                },
                "required": ["project", "pipeline_id"],
            },
        }),
        json!({
            "name": MR_READY,
            "description": "Mark a draft GitLab merge request ready, or with `ready: false` turn \
                            it back into a draft. A merge request already in that state is left \
                            as it is (`already: true`); a closed or merged one is refused. The \
                            operator is asked before it changes. An asked call returns `awaiting_operator` with an `approval_id` at once, and approval_status reports the outcome.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": PROJECT },
                    "iid": { "type": "integer" },
                    "ready": { "type": "boolean", "description": "true (the default) marks it ready; false makes it a draft again." },
                },
                "required": ["project", "iid"],
            },
        }),
        json!({
            "name": MR_MERGE,
            "description": "Merge a GitLab merge request only when its source SHA still matches the granted revision.",
            "inputSchema": { "type": "object", "properties": {
                "project": { "type": "string", "description": PROJECT }, "iid": { "type": "integer" },
                "head_sha": { "type": "string" }, "squash": { "type": "boolean" },
                "operation_id": { "type": "string" }
            }, "required": ["project", "iid", "head_sha", "operation_id"] },
        }),
        json!({
            "name": DEPLOY,
            "description": "Play one existing manual GitLab deployment job for an immutable pipeline SHA. The pipeline and job are verified before the broker plays it.",
            "inputSchema": { "type": "object", "properties": {
                "project": { "type": "string", "description": PROJECT }, "pipeline_id": { "type": "integer" },
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
    before_mutation: Option<&(dyn Fn() -> Result<(), String> + Send + Sync)>,
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
        MR_DISCUSSIONS => {
            let base = format!("{project_url}/merge_requests/{}", iid(args)?);
            let v = get(
                http,
                token,
                &format!("{base}/discussions?per_page={DISCUSSIONS_MAX}"),
            )
            .await?;
            let unresolved_only = args
                .get("unresolved_only")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let all = v.as_array().cloned().unwrap_or_default();
            let (threads, comments) = discussions(&all, unresolved_only);
            Ok(json!({
                "threads": threads,
                "more_threads": all.len() >= DISCUSSIONS_MAX,
                "comments": comments,
            }))
        }
        MR_REPLY => {
            let base = format!("{project_url}/merge_requests/{}", iid(args)?);
            let discussion = args
                .get("discussion_id")
                .and_then(Value::as_str)
                .filter(|d| {
                    !d.is_empty() && d.len() <= 64 && d.chars().all(|c| c.is_ascii_hexdigit())
                })
                .ok_or("discussion_id is required, as mr_discussions gave it")?;
            let body = args
                .get("body")
                .and_then(Value::as_str)
                .filter(|b| !b.trim().is_empty());
            let resolve = args
                .get("resolve")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if body.is_none() && !resolve {
                return Err("body is required unless the discussion is only being resolved".into());
            }
            if let Some(recheck) = before_mutation {
                recheck()?;
            }
            let mut out = json!({ "discussion_id": discussion });
            if let Some(body) = body {
                let v = write(
                    http.post(format!("{base}/discussions/{discussion}/notes")),
                    token,
                    &json!({ "body": body }),
                )
                .await?;
                out["note_id"] = v["id"].clone();
            }
            if resolve {
                let v = write(
                    http.put(format!("{base}/discussions/{discussion}")),
                    token,
                    &json!({ "resolved": true }),
                )
                .await?;
                out["resolved"] = json!(v["notes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|n| n["resolvable"] == true)
                    .all(|n| n["resolved"] == true));
            }
            Ok(out)
        }
        MR_READY => {
            let iid = iid(args)?;
            let ready = super::github::ready_arg(args)?;
            let url = format!("{project_url}/merge_requests/{iid}");
            let mr = get(http, token, &url).await?;
            let title = match ready_step(&mr, ready)? {
                ReadyStep::Already(out) => return Ok(out),
                ReadyStep::Retitle(title) => title,
            };
            if let Some(recheck) = before_mutation {
                recheck()?;
            }
            let v = write(http.put(&url), token, &json!({ "title": title })).await?;
            let draft = is_draft(&v);
            if draft == ready {
                return Err(format!(
                    "gitlab took the new title \"{title}\" but still reads !{iid} as {}",
                    if ready { "a draft" } else { "ready" }
                ));
            }
            Ok(json!({
                "iid": iid,
                "draft": draft,
                "head_sha": v["sha"],
                "web_url": v["web_url"],
                "already": false,
            }))
        }
        MR_FOR_BRANCH => {
            let branch = args
                .get("branch")
                .and_then(Value::as_str)
                .filter(|b| !b.is_empty())
                .ok_or("branch is required")?;
            let v = get(
                http,
                token,
                &format!(
                    "{project_url}/merge_requests?state=opened&source_branch={}&per_page=10",
                    urlencode(branch)
                ),
            )
            .await?;
            let open: Vec<Value> = v
                .as_array()
                .into_iter()
                .flatten()
                .map(|m| {
                    json!({ "iid": m["iid"], "title": m["title"], "draft": m["draft"],
                            "target_branch": m["target_branch"], "web_url": m["web_url"] })
                })
                .collect();
            Ok(json!({ "branch": branch, "open": open }))
        }
        MR_STATUS | MR_COMMENT | MR_MERGE => {
            let iid = args
                .get("iid")
                .and_then(Value::as_i64)
                .filter(|n| *n > 0)
                .ok_or("iid is required")?;
            if name == MR_MERGE {
                let sha = args
                    .get("head_sha")
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
                    .ok_or("head_sha is required")?;
                let mr = get(http, token, &format!("{project_url}/merge_requests/{iid}")).await?;
                if mr["sha"].as_str() != Some(sha) {
                    return Err("merge request source changed since the authorized revision".into());
                }
                if let Some(recheck) = before_mutation {
                    recheck()?;
                }
                let res = http
                    .put(format!("{project_url}/merge_requests/{iid}/merge"))
                    .header("PRIVATE-TOKEN", token)
                    .json(
                        &json!({ "sha": sha, "squash": args["squash"].as_bool().unwrap_or(true) }),
                    )
                    .send()
                    .await
                    .map_err(|e| format!("mutation-outcome-unknown: gitlab: {e}"))?;
                let status = res.status();
                let v: Value = res.json().await.unwrap_or(Value::Null);
                if !status.is_success() {
                    let error = format!("gitlab refused the merge ({status}): {}", v["message"]);
                    return Err(if status.is_server_error() {
                        format!("mutation-outcome-unknown: {error}")
                    } else {
                        error
                    });
                }
                Ok(
                    json!({ "state": v["state"], "merge_commit_sha": v["merge_commit_sha"], "web_url": v["web_url"] }),
                )
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
            pipeline(http, token, &project_url, id).await
        }
        PIPELINE_WAIT => {
            let id = args
                .get("pipeline_id")
                .and_then(Value::as_i64)
                .filter(|id| *id > 0)
                .ok_or("pipeline_id is required")?;
            let since = args.get("since").and_then(Value::as_str);
            let wait_until = super::wait::until(args)?;
            let until = tokio::time::Instant::now()
                + std::time::Duration::from_secs(super::wait::wait_secs(ctx, args));
            let mut baseline: Option<String> = since.map(str::to_string);
            loop {
                let mut now = pipeline(http, token, &project_url, id).await?;
                let read = wait_read(&now, baseline.as_deref(), wait_until);
                let left = until.saturating_duration_since(tokio::time::Instant::now());
                if read.returns || left.is_zero() {
                    now["state"] = Value::from(read.state);
                    now["changed"] = Value::from(read.changed);
                    now["finished"] = Value::from(read.done);
                    return Ok(now);
                }
                baseline.get_or_insert(read.state);
                tokio::time::sleep(left.min(WAIT_EVERY)).await;
            }
        }
        PIPELINE_LIST_BY_SHA => {
            let sha = args
                .get("sha")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or("sha is required")?;
            if !sha.bytes().all(|b| b.is_ascii_hexdigit()) || sha.len() < 7 || sha.len() > 64 {
                return Err("sha must be a commit hash".into());
            }
            let pipelines = get(
                http,
                token,
                &format!(
                    "{project_url}/pipelines?sha={}&order_by=id&sort=desc&per_page={SHA_PIPELINES_MAX}",
                    urlencode(sha)
                ),
            )
            .await?;
            let pipelines: Vec<Value> = pipelines
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter(|p| p["sha"].as_str().is_some_and(|s| s.eq_ignore_ascii_case(sha)))
                        .map(|p| json!({ "id": p["id"], "ref": p["ref"], "status": p["status"], "web_url": p["web_url"] }))
                        .collect()
                })
                .unwrap_or_default();
            Ok(Value::Array(pipelines))
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
        JOB_PLAY => {
            let job = args
                .get("job_id")
                .and_then(Value::as_i64)
                .ok_or("job_id is required")?;
            let variables = pipeline_variables(args.get("variables"))?;
            let res = http
                .post(format!("{project_url}/jobs/{job}/play"))
                .header("PRIVATE-TOKEN", token)
                .json(&json!({ "job_variables_attributes": variables }))
                .send()
                .await
                .map_err(|e| format!("gitlab: {e}"))?;
            let status = res.status();
            let v: Value = res.json().await.unwrap_or(Value::Null);
            if !status.is_success() {
                return Err(format!(
                    "gitlab refused to play job {job} ({status}): {}",
                    v["message"]
                ));
            }
            Ok(
                json!({ "id": v["id"], "status": v["status"], "web_url": v["web_url"], "pipeline_id": v["pipeline"]["id"] }),
            )
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
        PIPELINE_RETRY => {
            let id = args
                .get("pipeline_id")
                .and_then(Value::as_i64)
                .filter(|id| *id > 0)
                .ok_or("pipeline_id is required")?;
            let p = get(http, token, &format!("{project_url}/pipelines/{id}")).await?;
            if p["tag"] == true {
                return Err(format!(
                    "pipeline {id} ran on the tag {}; a pipeline on a tag is a release, and \
                     that is run by hand",
                    p["ref"]
                ));
            }
            // A retry runs with the variables the pipeline was started with,
            // so they are held to what pipeline_run would have accepted.
            let variables = get(
                http,
                token,
                &format!("{project_url}/pipelines/{id}/variables"),
            )
            .await
            .map_err(|e| format!("pipeline {id}'s variables could not be checked: {e}"))?;
            if let Some(name) = names_production(&variables) {
                return Err(format!(
                    "pipeline {id}'s variable {name} names production; a production deploy \
                     is run by hand"
                ));
            }
            if let Some(recheck) = before_mutation {
                recheck()?;
            }
            let v = write(
                http.post(format!("{project_url}/pipelines/{id}/retry")),
                token,
                &json!({}),
            )
            .await?;
            Ok(
                json!({ "id": v["id"], "status": v["status"], "web_url": v["web_url"],
                       "retried": "failed jobs" }),
            )
        }
        DEPLOY => {
            let pipeline_id = args
                .get("pipeline_id")
                .and_then(Value::as_i64)
                .filter(|id| *id > 0)
                .ok_or("pipeline_id is required")?;
            let job_id = args
                .get("job_id")
                .and_then(Value::as_i64)
                .filter(|id| *id > 0)
                .ok_or("job_id is required")?;
            let environment = args
                .get("environment")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| {
                    !v.is_empty()
                        && v.chars()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                })
                .ok_or("environment is required")?;
            let source_sha = args
                .get("source_sha")
                .and_then(Value::as_str)
                .filter(|v| v.len() >= 7)
                .ok_or("source_sha is required")?;
            let pipeline = get(
                http,
                token,
                &format!("{project_url}/pipelines/{pipeline_id}"),
            )
            .await?;
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
            let res = http
                .post(format!("{project_url}/jobs/{job_id}/play"))
                .header("PRIVATE-TOKEN", token)
                .json(&json!({}))
                .send()
                .await
                .map_err(|e| format!("mutation-outcome-unknown: gitlab: {e}"))?;
            let status = res.status();
            let v: Value = res.json().await.unwrap_or(Value::Null);
            if !status.is_success() {
                let error = format!(
                    "gitlab refused the deployment job ({status}): {}",
                    v["message"]
                );
                return Err(if status.is_server_error() {
                    format!("mutation-outcome-unknown: {error}")
                } else {
                    error
                });
            }
            Ok(
                json!({ "id": v["id"], "status": v["status"], "web_url": v["web_url"], "environment": environment, "source_sha": source_sha, "pipeline_id": pipeline_id }),
            )
        }
        other => Err(format!("no gitlab tool named {other}")),
    }
}

/// The prefixes GitLab reads a draft by, lowercased: `Draft:`, `[Draft]`
/// and `(Draft)`.
const DRAFT_PREFIXES: &[&str] = &["draft:", "[draft]", "(draft)"];

/// The `WIP` prefixes only an older GitLab reads a draft by. A current one
/// reads them as part of the title, so they are taken off when marking
/// ready, where an older GitLab would otherwise still see a draft, and left
/// alone when making a draft.
const WIP_PREFIXES: &[&str] = &["wip:", "[wip]"];

/// Whether GitLab reads a merge request as a draft: `draft`, or the
/// `work_in_progress` an older GitLab answers with instead.
fn is_draft(mr: &Value) -> bool {
    mr["draft"] == true || mr["work_in_progress"] == true
}

/// What `mr_ready` does with a merge request as GitLab just read it.
#[derive(Debug, PartialEq)]
enum ReadyStep {
    /// It is already in the state asked for: the answer, with no write.
    Already(Value),
    /// The title to set. GitLab's REST API has no draft field to write, so
    /// on every version a draft is marked by its title's prefix, as GitLab's
    /// own "Mark as ready" does.
    Retitle(String),
}

fn ready_step(mr: &Value, ready: bool) -> Result<ReadyStep, String> {
    let iid = &mr["iid"];
    match mr["state"].as_str() {
        Some("opened") => {}
        Some(state @ ("merged" | "closed")) => {
            return Err(format!(
                "merge request !{iid} is {state}; only an open one is marked ready or made a draft"
            ))
        }
        other => {
            return Err(format!(
                "merge request !{iid} is {}; only an open one is marked ready or made a draft",
                other.unwrap_or("in no state gitlab named")
            ))
        }
    }
    let draft = is_draft(mr);
    if draft != ready {
        return Ok(ReadyStep::Already(json!({
            "iid": iid,
            "draft": draft,
            "head_sha": mr["sha"],
            "web_url": mr["web_url"],
            "already": true,
        })));
    }
    let mut title = mr["title"].as_str().unwrap_or_default().trim();
    let wip: &[&str] = if ready { WIP_PREFIXES } else { &[] };
    loop {
        let lower = title.to_ascii_lowercase();
        let Some(prefix) = DRAFT_PREFIXES
            .iter()
            .chain(wip)
            .find(|p| lower.starts_with(*p))
        else {
            break;
        };
        title = title[prefix.len()..].trim_start();
    }
    Ok(ReadyStep::Retitle(if ready {
        title.to_string()
    } else {
        format!("Draft: {title}")
    }))
}

/// How often `pipeline_wait` reads the pipeline while it holds.
const WAIT_EVERY: std::time::Duration = std::time::Duration::from_secs(3);

/// A pipeline's status and its jobs, as `pipeline_status` returns them.
pub(crate) async fn pipeline(
    http: &reqwest::Client,
    token: &str,
    project_url: &str,
    id: i64,
) -> Result<Value, String> {
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
                    json!({
                        "id": j["id"], "name": j["name"], "stage": j["stage"],
                        "status": j["status"], "allow_failure": j["allow_failure"],
                    })
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

/// A pipeline status GitLab will not change on its own. `manual` is not one
/// of them for GitLab, but nothing moves until someone plays the job, so a
/// wait or a follow stops there too.
pub fn finished(status: &str) -> bool {
    matches!(
        status,
        "success" | "failed" | "canceled" | "skipped" | "manual"
    )
}

/// What one read of the pipeline means to `pipeline_wait`, against the
/// state the caller last saw.
fn wait_read(
    pipeline: &Value,
    baseline: Option<&str>,
    until: super::wait::Until,
) -> super::wait::Read {
    let state = state_token(pipeline);
    let changed = baseline.is_some_and(|b| b != state);
    let done = pipeline["status"].as_str().is_some_and(finished);
    let failed = super::wait::new_failure(&state, baseline);
    let returns = super::wait::returns(until, changed, done, failed);
    super::wait::Read {
        state,
        changed,
        done,
        returns,
    }
}

/// The pipeline's jobs that have failed in a way that fails it: one allowed
/// to fail does not count.
fn failed_jobs(pipeline: &Value) -> Vec<i64> {
    pipeline["jobs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|j| j["status"] == "failed" && j["allow_failure"] != true)
        .map(|j| j["id"].as_i64().unwrap_or(0))
        .collect()
}

/// What `pipeline_wait` compares: the pipeline's status and each job's.
fn state_token(pipeline: &Value) -> String {
    let mut seen = format!("{}", pipeline["status"]);
    let mut jobs: Vec<(i64, String)> = pipeline["jobs"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|j| (j["id"].as_i64().unwrap_or(0), j["status"].to_string()))
        .collect();
    jobs.sort();
    for (id, status) in jobs {
        seen.push_str(&format!(";{id}={status}"));
    }
    super::wait::state_token(&seen, failed_jobs(pipeline))
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
        // The newest note a person left; the count leaves out system notes.
        let last_note_at = match mr["user_notes_count"].as_u64() {
            Some(count) if count > 0 => get(
                http,
                token,
                &format!("{base}/notes?sort=desc&order_by=created_at&per_page=20"),
            )
            .await
            .ok()
            .and_then(|notes| {
                notes
                    .as_array()?
                    .iter()
                    .find(|note| note["system"] != true)
                    .map(|note| note["created_at"].clone())
            })
            .unwrap_or(Value::Null),
            _ => Value::Null,
        };
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
            "last_note_at": last_note_at,
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
            // Anything else would be sent as its JSON text, which is not
            // the value that was approved.
            let value = v
                .as_str()
                .ok_or_else(|| format!("variable {k} must be a string, e.g. \"{v}\""))?
                .to_string();
            if value.to_ascii_lowercase().contains("production") {
                return Err(format!(
                    "variable {k} names production; a production deploy is run by hand"
                ));
            }
            Ok(json!({ "key": k, "value": value }))
        })
        .collect()
}

/// The first of a pipeline's variables (GitLab's `[{key, value}]`) whose
/// value names production.
fn names_production(variables: &Value) -> Option<&str> {
    variables
        .as_array()
        .into_iter()
        .flatten()
        .find(|v| {
            v["value"]
                .as_str()
                .is_some_and(|value| value.to_ascii_lowercase().contains("production"))
        })
        .map(|v| v["key"].as_str().unwrap_or("?"))
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

fn iid(args: &Value) -> Result<i64, String> {
    args.get("iid")
        .and_then(Value::as_i64)
        .filter(|n| *n > 0)
        .ok_or_else(|| "iid is required".to_string())
}

/// Discussions as a reviewer reads them: threads anchored to the diff (or
/// started as resolvable), and general comments, with GitLab's own system
/// notes ("added 1 commit") left out.
fn discussions(all: &[Value], unresolved_only: bool) -> (Vec<Value>, Vec<Value>) {
    let mut threads = Vec::new();
    let mut comments = Vec::new();
    for d in all {
        let notes: Vec<&Value> = d["notes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|n| n["system"] != true)
            .collect();
        let Some(first) = notes.first() else { continue };
        let note = |n: &&Value| {
            json!({ "id": n["id"], "author": n["author"]["username"], "body": n["body"],
                    "created_at": n["created_at"] })
        };
        if d["individual_note"] == true && first["resolvable"] != true {
            comments.push(note(first));
            continue;
        }
        let resolvable: Vec<&&Value> = notes.iter().filter(|n| n["resolvable"] == true).collect();
        let resolved = !resolvable.is_empty() && resolvable.iter().all(|n| n["resolved"] == true);
        if unresolved_only && resolved {
            continue;
        }
        let position = &first["position"];
        threads.push(json!({
            "id": d["id"],
            "resolved": resolved,
            "path": if position["new_path"].is_null() { position["old_path"].clone() } else { position["new_path"].clone() },
            "line": if position["new_line"].is_null() { position["old_line"].clone() } else { position["new_line"].clone() },
            "notes": notes.iter().map(note).collect::<Vec<_>>(),
        }));
    }
    (threads, comments)
}

/// A write whose outcome is unknown when GitLab cannot be reached or fails
/// on its side: the note may or may not exist.
async fn write(
    request: reqwest::RequestBuilder,
    token: &str,
    body: &Value,
) -> Result<Value, String> {
    let res = request
        .header("PRIVATE-TOKEN", token)
        .json(body)
        .send()
        .await
        .map_err(|e| format!("mutation-outcome-unknown: gitlab: {e}"))?;
    let status = res.status();
    let v: Value = res.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let error = format!("gitlab refused ({status}): {}", v["message"]);
        return Err(if status.is_server_error() {
            format!("mutation-outcome-unknown: {error}")
        } else {
            error
        });
    }
    Ok(v)
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
    fn discussions_split_threads_from_comments_and_drop_system_notes() {
        let all = [
            json!({ "id": "a1", "individual_note": true, "notes": [
                { "id": 1, "body": "looks good", "author": { "username": "r" } }] }),
            json!({ "id": "b2", "individual_note": true, "notes": [
                { "id": 2, "body": "added 1 commit", "system": true }] }),
            json!({ "id": "c3", "individual_note": false, "notes": [
                { "id": 3, "body": "rename this", "resolvable": true, "resolved": false,
                  "author": { "username": "r" },
                  "position": { "new_path": "src/a.rs", "new_line": 12 } },
                { "id": 4, "body": "done", "resolvable": true, "resolved": false }] }),
            json!({ "id": "d4", "individual_note": false, "notes": [
                { "id": 5, "body": "nit", "resolvable": true, "resolved": true,
                  "position": { "old_path": "src/b.rs", "old_line": 3 } }] }),
        ];
        let (threads, comments) = discussions(&all, false);
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0]["body"], "looks good");
        assert_eq!(threads.len(), 2);
        assert_eq!(threads[0]["path"], "src/a.rs");
        assert_eq!(threads[0]["line"], 12);
        assert_eq!(threads[0]["resolved"], false);
        assert_eq!(threads[0]["notes"].as_array().unwrap().len(), 2);
        assert_eq!(threads[1]["path"], "src/b.rs");
        let (threads, _) = discussions(&all, true);
        assert_eq!(threads.len(), 1, "a resolved thread is left out when asked");
    }

    #[test]
    fn ready_rewrites_the_draft_prefix_and_leaves_one_already_there_alone() {
        let mr = |title: &str, draft: bool| {
            json!({ "iid": 9, "state": "opened", "title": title, "draft": draft,
                    "sha": "abc123", "web_url": "https://gitlab.test/g/p/-/merge_requests/9" })
        };
        for title in [
            "Draft: Add thing",
            "[Draft] Add thing",
            "(draft)Add thing",
            "Draft: WIP: Add thing",
        ] {
            assert_eq!(
                ready_step(&mr(title, true), true).unwrap(),
                ReadyStep::Retitle("Add thing".into()),
                "{title}"
            );
        }
        assert_eq!(
            ready_step(&mr("Add thing", false), false).unwrap(),
            ReadyStep::Retitle("Draft: Add thing".into())
        );
        // `WIP` is a draft only to an older GitLab: taken off when marking
        // ready, kept as the title's own words when making a draft.
        assert_eq!(
            ready_step(&mr("[WIP] Add thing", true), true).unwrap(),
            ReadyStep::Retitle("Add thing".into())
        );
        assert_eq!(
            ready_step(&mr("[WIP] Add thing", false), false).unwrap(),
            ReadyStep::Retitle("Draft: [WIP] Add thing".into())
        );
        let old = json!({ "iid": 9, "state": "opened", "title": "WIP: Add thing",
                          "work_in_progress": true });
        assert_eq!(
            ready_step(&old, true).unwrap(),
            ReadyStep::Retitle("Add thing".into())
        );
        let ReadyStep::Already(already) = ready_step(&mr("Add thing", false), true).unwrap() else {
            panic!("a ready merge request is written to again");
        };
        assert_eq!(already["already"], true);
        assert_eq!(already["draft"], false);
        assert_eq!(already["head_sha"], "abc123");
        assert!(matches!(
            ready_step(&mr("Draft: x", true), false).unwrap(),
            ReadyStep::Already(_)
        ));
    }

    #[test]
    fn ready_refuses_a_closed_or_merged_merge_request() {
        for state in ["closed", "merged"] {
            let mr = json!({ "iid": 9, "state": state, "title": "Draft: x", "draft": true });
            let e = ready_step(&mr, true).unwrap_err();
            assert!(e.contains(&format!("is {state}")), "{e}");
        }
    }

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
        let ok = pipeline_variables(Some(&json!({ "DEPLOY": "staging", "N": "2" }))).unwrap();
        assert_eq!(ok.len(), 2);
        assert!(pipeline_variables(None).unwrap().is_empty());
    }

    #[test]
    fn a_retried_pipeline_keeps_its_variables_so_they_are_held_to_the_same_rule() {
        let vars = json!([
            { "key": "N", "value": "2" },
            { "key": "TARGET", "value": "eu-PRODUCTION" },
        ]);
        assert_eq!(names_production(&vars), Some("TARGET"));
        assert_eq!(
            names_production(&json!([{ "key": "N", "value": "2" }])),
            None
        );
        assert_eq!(names_production(&Value::Null), None);
    }

    #[test]
    fn a_variable_that_is_not_a_string_is_refused_rather_than_converted() {
        for value in [json!(2), json!(true), json!({ "a": 1 }), json!(["x"])] {
            let e = pipeline_variables(Some(&json!({ "N": value }))).unwrap_err();
            assert!(e.contains("N must be a string"), "{e}");
        }
        let ok = pipeline_variables(Some(&json!({ "N": " 2 " }))).unwrap();
        assert_eq!(ok, vec![json!({ "key": "N", "value": " 2 " })]);
    }

    /// Feeds a scripted run of pipeline reads through `pipeline_wait`'s
    /// decision, carrying the state forward as one call does, and says at
    /// which read it returns.
    fn returns_at(
        reads: &[Value],
        since: Option<&str>,
        until: super::super::wait::Until,
    ) -> Option<usize> {
        let mut baseline = since.map(str::to_string);
        for (i, now) in reads.iter().enumerate() {
            let read = wait_read(now, baseline.as_deref(), until);
            if read.returns {
                return Some(i);
            }
            baseline.get_or_insert(read.state);
        }
        None
    }

    fn pipeline_read(status: &str, jobs: &[(i64, &str, bool)]) -> Value {
        let jobs: Vec<Value> = jobs
            .iter()
            .map(
                |(id, status, allow)| json!({ "id": id, "status": status, "allow_failure": allow }),
            )
            .collect();
        json!({ "status": status, "jobs": jobs })
    }

    #[test]
    fn a_wait_until_finished_holds_through_job_moves_and_a_failure() {
        use super::super::wait::Until;
        let reads = [
            pipeline_read("running", &[(1, "pending", false), (2, "created", false)]),
            pipeline_read("running", &[(1, "running", false), (2, "created", false)]),
            pipeline_read("running", &[(1, "failed", false), (2, "pending", false)]),
            pipeline_read("running", &[(1, "failed", false), (2, "running", false)]),
            pipeline_read("failed", &[(1, "failed", false), (2, "success", false)]),
        ];
        assert_eq!(returns_at(&reads, None, Until::Change), Some(1));
        assert_eq!(returns_at(&reads, None, Until::Finished), Some(4));
        assert_eq!(returns_at(&reads, None, Until::Failure), Some(2));
    }

    #[test]
    fn a_wait_until_failure_ignores_a_job_allowed_to_fail() {
        use super::super::wait::Until;
        let reads = [
            pipeline_read("running", &[(1, "running", true), (2, "running", false)]),
            pipeline_read("running", &[(1, "failed", true), (2, "running", false)]),
            pipeline_read("success", &[(1, "failed", true), (2, "success", false)]),
        ];
        assert_eq!(returns_at(&reads, None, Until::Failure), Some(2));
    }

    #[test]
    fn a_wait_until_failure_reports_a_failed_job_once_rather_than_on_every_call() {
        use super::super::wait::Until;
        let failed = pipeline_read("running", &[(1, "failed", false), (2, "running", false)]);
        // A first look already sees the failure, and says so at once.
        assert_eq!(
            returns_at(std::slice::from_ref(&failed), None, Until::Failure),
            Some(0)
        );
        // Handed that state back, the next call holds while nothing moves.
        let seen = state_token(&failed);
        assert_eq!(
            returns_at(
                &[failed.clone(), failed.clone()],
                Some(&seen),
                Until::Failure
            ),
            None
        );
        // A failure the caller missed between calls still ends the wait.
        let before = pipeline_read("running", &[(1, "running", false), (2, "running", false)]);
        assert_eq!(
            returns_at(&[failed], Some(&state_token(&before)), Until::Failure),
            Some(0)
        );
    }

    #[test]
    fn a_wait_until_failure_holds_through_later_moves_once_told_and_returns_on_another_failure() {
        use super::super::wait::Until;
        let told = pipeline_read(
            "running",
            &[
                (1, "failed", false),
                (2, "running", false),
                (3, "running", false),
            ],
        );
        let seen = state_token(&told);
        let moved = pipeline_read(
            "running",
            &[
                (1, "failed", false),
                (2, "success", false),
                (3, "running", false),
            ],
        );
        assert_eq!(
            returns_at(std::slice::from_ref(&moved), Some(&seen), Until::Change),
            Some(0)
        );
        assert_eq!(
            returns_at(std::slice::from_ref(&moved), Some(&seen), Until::Failure),
            None
        );
        let again = pipeline_read(
            "running",
            &[
                (1, "failed", false),
                (2, "success", false),
                (3, "failed", false),
            ],
        );
        assert_eq!(
            returns_at(&[moved, again], Some(&seen), Until::Failure),
            Some(1)
        );
    }

    #[test]
    fn a_wait_until_change_still_returns_on_the_first_move_and_not_on_a_first_look() {
        use super::super::wait::Until;
        let still = pipeline_read("running", &[(1, "failed", false)]);
        assert_eq!(
            returns_at(&[still.clone(), still], None, Until::Change),
            None
        );
    }
}
