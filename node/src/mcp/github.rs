//! GitHub, as narrow tools: a pull request's state with its checks and
//! reviews, its review threads, one comment on it, one reply to a thread, one
//! comment on an issue, marking it ready for review or back to draft (asked),
//! the open pull request for a branch, the Actions runs for a branch or an exact
//! commit with each run's jobs, waiting for a run to move, the end of a job's
//! log, rerunning a run's failed jobs (asked), and starting a workflow on a
//! branch (asked, and only a workflow the node's `[github] dispatch` lists).
//! That is what GitLab's pipeline tools give an agent, so CI is diagnosed,
//! retried, started and followed the same way on either forge
//! (`super::parity` holds the mapping and fails a test when a tool has no
//! decision). Opening a
//! pull request is the review path (`review::publish`); merging is
//! `pr_merge`, which runs only with current scoped authority for the exact
//! head SHA. The token never leaves the node.

use serde_json::{json, Value};

use crate::{broker::SharedBroker, forge::Forge, mcp::CallContext};

pub const CREDENTIAL: &str = "gh";
pub const PR_STATUS: &str = "pr_status";
pub const PR_COMMENT: &str = "pr_comment";
/// Named for the forge, not `gh issue comment`'s `issue_comment`: that is
/// Jira's tool, and a channel can hold both (#593).
pub const ISSUE_COMMENT: &str = "github_issue_comment";
pub const RUN_STATUS: &str = "run_status";
pub const PR_MERGE: &str = "pr_merge";
pub const PR_READY: &str = "pr_ready";
pub const PR_THREADS: &str = "pr_threads";
pub const PR_REPLY: &str = "pr_reply";
pub const PR_FOR_BRANCH: &str = "pr_for_branch";
pub const RUN_LOGS: &str = "run_logs";
pub const RUN_RERUN: &str = "run_rerun";
pub const RUN_WAIT: &str = "run_wait";
pub const RUN_DISPATCH: &str = "run_dispatch";

/// Every tool here, each of which takes the repository as `repo`.
pub const TOOLS: &[&str] = &[
    PR_STATUS,
    PR_COMMENT,
    ISSUE_COMMENT,
    RUN_STATUS,
    PR_MERGE,
    PR_READY,
    PR_THREADS,
    PR_REPLY,
    PR_FOR_BRANCH,
    RUN_LOGS,
    RUN_RERUN,
    RUN_WAIT,
    RUN_DISPATCH,
];

/// What `repo` is. Optional in what a session on a GitHub repository is
/// offered (`Tools::list_offered`), required everywhere else.
const REPO: &str = "owner/name. In a session on a GitHub repository, defaults to that \
                    repository; required outside a session.";

/// How much of a job's log `run_logs` returns, in KiB, unless asked: the same
/// limits as GitLab's `job_trace`.
const LOG_KIB: u64 = 16;
const LOG_KIB_MAX: u64 = 64;

/// One page of threads, and of comments in each: a review is read in one
/// call, and a pull request with more than this says so rather than
/// pretending it has none.
const THREADS_MAX: u32 = 100;
const THREAD_COMMENTS_MAX: u32 = 50;

const THREADS_QUERY: &str =
    "query($owner: String!, $name: String!, $number: Int!, $threads: Int!, $comments: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: $threads) {
        pageInfo { hasNextPage }
        nodes {
          id isResolved isOutdated path line originalLine
          comments(first: $comments) {
            nodes { author { login } body createdAt url }
          }
        }
      }
    }
  }
}";

const REPLY_MUTATION: &str = "mutation($thread: ID!, $body: String!) {
  addPullRequestReviewThreadReply(input: { pullRequestReviewThreadId: $thread, body: $body }) {
    comment { url }
  }
}";

/// REST cannot clear `draft` on a pull request; GraphQL can, both ways.
const READY_MUTATION: &str = "mutation($id: ID!) {
  markPullRequestReadyForReview(input: { pullRequestId: $id }) {
    pullRequest { isDraft headRefOid url }
  }
}";

const DRAFT_MUTATION: &str = "mutation($id: ID!) {
  convertPullRequestToDraft(input: { pullRequestId: $id }) {
    pullRequest { isDraft headRefOid url }
  }
}";

const RESOLVE_MUTATION: &str = "mutation($thread: ID!) {
  resolveReviewThread(input: { threadId: $thread }) { thread { isResolved } }
}";

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": PR_STATUS,
            "description": "The state of a GitHub pull request: open/closed/merged, draft, \
                            mergeability, its checks rolled up (passed, failed, pending), and \
                            each reviewer's latest review with the decision they add up to.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": REPO },
                    "number": { "type": "integer" },
                },
                "required": ["repo", "number"],
            },
        }),
        json!({
            "name": PR_COMMENT,
            "description": "Post one comment on a GitHub pull request. For a GitHub issue use \
                            github_issue_comment. The operator is asked before it posts. An asked call returns `awaiting_operator` with an `approval_id` at once, and approval_status reports the outcome.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": REPO },
                    "number": { "type": "integer" },
                    "body": { "type": "string" },
                },
                "required": ["repo", "number", "body"],
            },
        }),
        json!({
            "name": ISSUE_COMMENT,
            "description": "Post one comment on a GitHub issue (`number` is the issue's number \
                            in `repo`). Not for Jira issues, and not for pull requests, which are \
                            pr_comment. The operator is asked before it posts. An asked call returns `awaiting_operator` with an `approval_id` at once, and approval_status reports the outcome.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": REPO },
                    "number": { "type": "integer", "description": "The issue's number." },
                    "body": { "type": "string" },
                },
                "required": ["repo", "number", "body"],
            },
        }),
        json!({
            "name": RUN_STATUS,
            "description": "GitHub Actions runs: the latest for a branch, every run at an exact \
                            commit SHA (never resolved against a branch), or one run by id with \
                            its jobs (id, name, status, conclusion, the steps that failed). Pass a \
                            failed job's id to run_logs.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": REPO },
                    "branch": { "type": "string" },
                    "sha": { "type": "string", "description": "An exact commit SHA." },
                    "run_id": { "type": "integer", "description": "One run, with its jobs." },
                },
                "required": ["repo"],
            },
        }),
        json!({
            "name": RUN_LOGS,
            "description": "The end of a GitHub Actions job's log, for reading why it failed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": REPO },
                    "job_id": { "type": "integer", "description": "From run_status with run_id." },
                    "kib": { "type": "integer", "description": "How much of the end, in KiB (16 unless you say, at most 64)." },
                },
                "required": ["repo", "job_id"],
            },
        }),
        json!({
            "name": RUN_RERUN,
            "description": "Rerun a GitHub Actions run's failed jobs, as the web UI's \"Re-run \
                            failed jobs\" does. The operator is asked before this runs. An asked call returns `awaiting_operator` with an `approval_id` at once, and approval_status reports the outcome. Once it ran, the node follows \
                            the new attempt and run_wait waits for it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": REPO },
                    "run_id": { "type": "integer" },
                },
                "required": ["repo", "run_id"],
            },
        }),
        json!({
            "name": RUN_DISPATCH,
            "description": "Start a GitHub Actions workflow on a branch through its \
                            workflow_dispatch trigger, as the web UI's \"Run workflow\" does. \
                            Only a workflow the node's configuration lists for this repository, \
                            never on a tag, and never with an input naming production: those \
                            are run by hand. The operator is asked before this runs. An asked call returns `awaiting_operator` with an `approval_id` at once, and approval_status reports the outcome. Once it ran, the node follows \
                            the run it started and run_wait waits for it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": REPO },
                    "workflow": { "type": "string", "description": "The workflow's file name \
                        in .github/workflows, e.g. renovate.yaml." },
                    "ref": { "type": "string", "description": "The branch." },
                    "inputs": { "type": "object", "description": "Workflow inputs, name to \
                        string value. Values are sent as given; a number or boolean is refused, \
                        so quote it." },
                },
                "required": ["repo", "workflow", "ref"],
            },
        }),
        json!({
            "name": RUN_WAIT,
            "description": format!(
                "Wait for a GitHub Actions run to move: returns as soon as it or one of its \
                 jobs changes state, when it has completed, or after wait_secs, with the same \
                 run and jobs as run_status with run_id and a `state` token. Pass that token \
                 back as `since` so a change between calls is not missed. `until: \"finished\"` \
                 holds through job moves until the run completes, and `until: \"failure\"` \
                 until then or a job fails. A failed job's log is run_logs. {}",
                super::wait::pointer()
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": REPO },
                    "run_id": { "type": "integer" },
                    "attempt": { "type": "integer", "description": "The attempt run_rerun \
                        started: an earlier attempt's result is not taken for this one's." },
                    "since": { "type": "string", "description": "The `state` an earlier call returned." },
                    "until": super::wait::until_schema(),
                    "wait_secs": { "type": "integer", "description": "How long to hold, at most 45 (the default)." },
                },
                "required": ["repo", "run_id"],
            },
        }),
        json!({
            "name": PR_THREADS,
            "description": "A GitHub pull request's review feedback: its review threads (id, \
                            resolved, outdated, file and line, each comment) and its general \
                            conversation comments. Pass a thread's id to pr_reply.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": REPO },
                    "number": { "type": "integer" },
                    "unresolved_only": { "type": "boolean", "description": "Leave out resolved threads." },
                },
                "required": ["repo", "number"],
            },
        }),
        json!({
            "name": PR_REPLY,
            "description": "Reply to one review thread on a GitHub pull request, and optionally \
                            resolve it. The operator is asked before it posts. An asked call returns `awaiting_operator` with an `approval_id` at once, and approval_status reports the outcome.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": REPO },
                    "number": { "type": "integer" },
                    "thread_id": { "type": "string", "description": "The thread's id from pr_threads." },
                    "body": { "type": "string", "description": "The reply. May be omitted when only resolving." },
                    "resolve": { "type": "boolean", "description": "Mark the thread resolved after replying." },
                },
                "required": ["repo", "number", "thread_id"],
            },
        }),
        json!({
            "name": PR_FOR_BRANCH,
            "description": "The open GitHub pull request for a branch of this repository, if \
                            there is one: its number is what submit_review takes as `change`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": REPO },
                    "branch": { "type": "string" },
                },
                "required": ["repo", "branch"],
            },
        }),
        json!({
            "name": PR_READY,
            "description": "Mark a draft GitHub pull request ready for review, or with `ready: \
                            false` turn it back into a draft. A pull request already in that \
                            state is left as it is (`already: true`); a closed or merged one is \
                            refused. The operator is asked before it changes. An asked call returns `awaiting_operator` with an `approval_id` at once, and approval_status reports the outcome.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": REPO },
                    "number": { "type": "integer" },
                    "ready": { "type": "boolean", "description": "true (the default) marks it ready for review; false makes it a draft again." },
                },
                "required": ["repo", "number"],
            },
        }),
        json!({
            "name": PR_MERGE,
            "description": "Merge one GitHub pull request at the exact head SHA. This only runs with current scoped authority.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": REPO },
                    "number": { "type": "integer" },
                    "head_sha": { "type": "string", "description": "The reviewed pull request head SHA." },
                    "method": { "type": "string", "enum": ["merge", "squash", "rebase"] },
                    "operation_id": { "type": "string", "description": "Stable idempotency id for this merge." },
                },
                "required": ["repo", "number", "head_sha", "operation_id"],
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
    before_mutation: Option<&(dyn Fn() -> Result<(), String> + Send + Sync)>,
) -> Result<Value, String> {
    let env = broker
        .read()
        .unwrap()
        .env_for(CREDENTIAL, &ctx.channel, &ctx.node_id)
        .map_err(|e| e.to_string())?;
    let token = Forge::Github
        .token(&env)
        .ok_or("credential gh has no GH_TOKEN")?;
    let api = env
        .get("GITHUB_API")
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| "https://api.github.com".into());
    let repo = repo(args)?;
    let base = format!("{api}/repos/{repo}");
    let graphql = graphql_url(&api);
    let gh = Client { http, token };
    match name {
        PR_STATUS => {
            let n = number(args)?;
            let pr = gh.get(&format!("{base}/pulls/{n}")).await?;
            let sha = pr["head"]["sha"].as_str().unwrap_or_default();
            let runs = gh
                .get(&format!("{base}/commits/{sha}/check-runs?per_page=100"))
                .await
                .unwrap_or(Value::Null);
            let reviews = gh
                .get(&format!("{base}/pulls/{n}/reviews?per_page=100"))
                .await
                .unwrap_or(Value::Null);
            let (decision, latest) = review_decision(&reviews);
            // The newest comment is the last page of one.
            let last_comment_at = match pr["comments"].as_u64() {
                Some(count) if count > 0 => gh
                    .get(&format!(
                        "{base}/issues/{n}/comments?per_page=1&page={count}"
                    ))
                    .await
                    .map(|page| page[0]["created_at"].clone())
                    .unwrap_or(Value::Null),
                _ => Value::Null,
            };
            Ok(json!({
                "number": pr["number"],
                "title": pr["title"],
                "state": pr["state"],
                "draft": pr["draft"],
                "merged": pr["merged"],
                "mergeable": pr["mergeable"],
                "mergeable_state": pr["mergeable_state"],
                "head": pr["head"]["ref"],
                "base": pr["base"]["ref"],
                "comments": pr["comments"],
                "last_comment_at": last_comment_at,
                "url": pr["html_url"],
                "checks": rollup(&runs),
                "review_decision": decision,
                "reviews": latest,
            }))
        }
        PR_THREADS => {
            let n = number(args)?;
            let (owner, name) = repo.split_once('/').expect("validated owner/name");
            let v = gh
                .graphql(
                    &graphql,
                    THREADS_QUERY,
                    json!({ "owner": owner, "name": name, "number": n,
                            "threads": THREADS_MAX, "comments": THREAD_COMMENTS_MAX }),
                )
                .await?;
            let threads = &v["data"]["repository"]["pullRequest"]["reviewThreads"];
            let unresolved_only = args
                .get("unresolved_only")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let listed: Vec<Value> = threads["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|t| !(unresolved_only && t["isResolved"] == true))
                .map(|t| {
                    json!({
                        "id": t["id"],
                        "resolved": t["isResolved"],
                        "outdated": t["isOutdated"],
                        "path": t["path"],
                        "line": if t["line"].is_null() { t["originalLine"].clone() } else { t["line"].clone() },
                        "comments": t["comments"]["nodes"].as_array().into_iter().flatten().map(|c| json!({
                            "author": c["author"]["login"], "body": c["body"],
                            "created_at": c["createdAt"], "url": c["url"],
                        })).collect::<Vec<_>>(),
                    })
                })
                .collect();
            let comments = gh
                .get(&format!("{base}/issues/{n}/comments?per_page=100"))
                .await?;
            let comments: Vec<Value> = comments
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| {
                    json!({ "id": c["id"], "author": c["user"]["login"], "body": c["body"],
                            "created_at": c["created_at"], "url": c["html_url"] })
                })
                .collect();
            Ok(json!({
                "threads": listed,
                "more_threads": threads["pageInfo"]["hasNextPage"] == true,
                "comments": comments,
            }))
        }
        PR_REPLY => {
            number(args)?;
            let thread = args
                .get("thread_id")
                .and_then(Value::as_str)
                .filter(|t| is_node_id(t))
                .ok_or("thread_id is required, as pr_threads gave it")?;
            let body = args
                .get("body")
                .and_then(Value::as_str)
                .filter(|b| !b.trim().is_empty());
            let resolve = args
                .get("resolve")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if body.is_none() && !resolve {
                return Err("body is required unless the thread is only being resolved".into());
            }
            if let Some(recheck) = before_mutation {
                recheck()?;
            }
            let mut out = json!({ "thread_id": thread });
            if let Some(body) = body {
                let v = gh
                    .graphql(
                        &graphql,
                        REPLY_MUTATION,
                        json!({ "thread": thread, "body": body }),
                    )
                    .await
                    .map_err(mutation_outcome_error)?;
                out["url"] = v["data"]["addPullRequestReviewThreadReply"]["comment"]["url"].clone();
            }
            if resolve {
                let v = gh
                    .graphql(&graphql, RESOLVE_MUTATION, json!({ "thread": thread }))
                    .await
                    .map_err(mutation_outcome_error)?;
                out["resolved"] = v["data"]["resolveReviewThread"]["thread"]["isResolved"].clone();
            }
            Ok(out)
        }
        PR_FOR_BRANCH => {
            let branch = args
                .get("branch")
                .and_then(Value::as_str)
                .filter(|b| is_ref(b))
                .ok_or("branch is required")?;
            let owner = repo.split('/').next().unwrap_or_default();
            let v = gh
                .get(&format!(
                    "{base}/pulls?state=open&head={owner}:{branch}&per_page=10"
                ))
                .await?;
            let open: Vec<Value> = v
                .as_array()
                .into_iter()
                .flatten()
                .map(|p| {
                    json!({ "number": p["number"], "title": p["title"], "draft": p["draft"],
                            "base": p["base"]["ref"], "url": p["html_url"] })
                })
                .collect();
            Ok(json!({ "branch": branch, "open": open }))
        }
        // GitHub comments on an issue and on a pull request through the same
        // endpoint; the two tools differ in what they tell the agent.
        PR_COMMENT | ISSUE_COMMENT => {
            let n = number(args)?;
            let body = args
                .get("body")
                .and_then(Value::as_str)
                .filter(|b| !b.trim().is_empty())
                .ok_or("body is required")?;
            let v = gh
                .post(
                    &format!("{base}/issues/{n}/comments"),
                    &json!({ "body": body }),
                )
                .await?;
            Ok(json!({ "id": v["id"], "url": v["html_url"] }))
        }
        PR_READY => {
            let n = number(args)?;
            let ready = ready_arg(args)?;
            let pr = gh.get(&format!("{base}/pulls/{n}")).await?;
            let (mutation, id) = match ready_step(&pr, ready)? {
                ReadyStep::Already(out) => return Ok(out),
                ReadyStep::Mutate { mutation, id } => (mutation, id),
            };
            if let Some(recheck) = before_mutation {
                recheck()?;
            }
            let v = gh
                .graphql(&graphql, mutation, json!({ "id": id }))
                .await
                .map_err(mutation_outcome_error)?;
            let field = if ready {
                "markPullRequestReadyForReview"
            } else {
                "convertPullRequestToDraft"
            };
            let after = &v["data"][field]["pullRequest"];
            if after["isDraft"].as_bool() != Some(!ready) {
                return Err(format!(
                    "github took the mutation but still reads #{n} as {}",
                    if ready { "a draft" } else { "ready" }
                ));
            }
            Ok(json!({
                "number": n,
                "draft": after["isDraft"],
                "head_sha": after["headRefOid"],
                "url": after["url"],
                "already": false,
            }))
        }
        PR_MERGE => {
            let n = number(args)?;
            let head_sha = args
                .get("head_sha")
                .and_then(Value::as_str)
                .filter(|s| s.len() >= 7)
                .ok_or("head_sha is required")?;
            let method = args
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("squash");
            if !matches!(method, "merge" | "squash" | "rebase") {
                return Err("method must be merge, squash, or rebase".into());
            }
            // Merges into one repository run one at a time, so a batch of
            // approvals does not race itself into GitHub's "Base branch was
            // modified". A merge from elsewhere can still land in between;
            // that answer means nothing was merged, so it is retried while
            // the head is still the authorized one.
            let lock = merge_lock(&base);
            let _held = lock.lock().await;
            let mut attempt = 0;
            loop {
                let pr = gh.get(&format!("{base}/pulls/{n}")).await?;
                if pr["head"]["sha"].as_str() != Some(head_sha) {
                    return Err("pull request head changed since the authorized revision".into());
                }
                if let Some(recheck) = before_mutation {
                    recheck()?;
                }
                match gh
                    .put(
                        &format!("{base}/pulls/{n}/merge"),
                        &json!({ "sha": head_sha, "merge_method": method }),
                    )
                    .await
                {
                    Ok(v) => {
                        return Ok(
                            json!({ "merged": v["merged"], "sha": v["sha"], "message": v["message"] }),
                        )
                    }
                    Err(e) if base_moved(&e) && attempt < MERGE_RETRIES => {
                        attempt += 1;
                        tokio::time::sleep(MERGE_RETRY_AFTER * attempt).await;
                    }
                    Err(e) => return Err(mutation_outcome_error(e)),
                }
            }
        }
        RUN_STATUS if args.get("run_id").is_some() => {
            let run = positive(args, "run_id")?;
            run_with_jobs(&gh, &base, run).await
        }
        RUN_WAIT => {
            let run = positive(args, "run_id")?;
            let attempt = args.get("attempt").and_then(Value::as_i64).unwrap_or(0);
            let since = args.get("since").and_then(Value::as_str);
            let wait_until = super::wait::until(args)?;
            let until = tokio::time::Instant::now()
                + std::time::Duration::from_secs(super::wait::wait_secs(args));
            let mut baseline: Option<String> = since.map(str::to_string);
            loop {
                let mut now = run_with_jobs(&gh, &base, run).await?;
                let read = wait_read(&now, attempt, baseline.as_deref(), wait_until);
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
        RUN_LOGS => {
            let job = positive(args, "job_id")?;
            let kib = args
                .get("kib")
                .and_then(Value::as_u64)
                .unwrap_or(LOG_KIB)
                .clamp(1, LOG_KIB_MAX);
            let log = gh.text(&format!("{base}/actions/jobs/{job}/logs")).await?;
            let (tail, truncated) = tail(&log, (kib * 1024) as usize);
            Ok(json!({ "job_id": job, "truncated": truncated, "log": tail }))
        }
        RUN_RERUN => {
            let run = positive(args, "run_id")?;
            // The attempt this starts is the one after the run's latest, so
            // a wait or a follow does not take the last attempt's result for
            // its own.
            let before = gh.get(&format!("{base}/actions/runs/{run}")).await?;
            if let Some(recheck) = before_mutation {
                recheck()?;
            }
            gh.post_empty(&format!("{base}/actions/runs/{run}/rerun-failed-jobs"))
                .await
                .map_err(mutation_outcome_error)?;
            let attempt = before["run_attempt"].as_i64().map(|a| a + 1);
            Ok(json!({ "run_id": run, "rerun": "failed jobs", "attempt": attempt }))
        }
        RUN_DISPATCH => {
            let Dispatch {
                workflow,
                branch,
                inputs,
            } = dispatch_request(args)?;
            // A name that is a tag is a release, and GitHub would resolve a
            // name that is both; either way it is run by hand.
            if gh
                .get(&format!("{base}/git/ref/tags/{branch}"))
                .await
                .is_ok()
            {
                return Err(format!(
                    "{branch} is a tag; a workflow on a tag is a release, and that is run by hand"
                ));
            }
            gh.get(&format!("{base}/branches/{branch}"))
                .await
                .map_err(|e| format!("{branch} is not a branch of {repo}: {e}"))?;
            let runs = format!(
                "{base}/actions/workflows/{workflow}/runs?branch={branch}\
                 &event=workflow_dispatch&per_page=20"
            );
            let before = gh
                .get(&runs)
                .await
                .map_err(|e| format!("no workflow {workflow} in {repo}: {e}"))?;
            if let Some(recheck) = before_mutation {
                recheck()?;
            }
            let v = gh
                .post(
                    &format!("{base}/actions/workflows/{workflow}/dispatches"),
                    &json!({ "ref": branch, "inputs": inputs, "return_run_details": true }),
                )
                .await
                .map_err(mutation_outcome_error)?;
            let mut started = v["workflow_run_id"]
                .as_i64()
                .map(|id| (id, v["html_url"].clone()));
            // A GitHub that answers 204 without the run (an older Enterprise
            // Server) leaves the node to find it: the newest dispatched run
            // of this workflow on this branch that was not there before.
            for _ in 0..FIND_TRIES {
                if started.is_some() {
                    break;
                }
                tokio::time::sleep(FIND_EVERY).await;
                if let Ok(now) = gh.get(&runs).await {
                    started = new_run(&before, &now);
                }
            }
            let mut out = json!({ "repo": repo, "workflow": workflow, "ref": branch });
            match started {
                Some((id, url)) => {
                    out["run_id"] = Value::from(id);
                    out["url"] = url;
                }
                None => {
                    out["run_id"] = Value::Null;
                    out["note"] = Value::from(
                        "GitHub started the workflow but did not say which run, and none \
                         appeared yet; run_status with this branch finds it.",
                    );
                }
            }
            Ok(out)
        }
        RUN_STATUS => {
            let filter = match (
                args.get("branch").and_then(Value::as_str),
                args.get("sha").and_then(Value::as_str),
            ) {
                (_, Some(sha)) if is_ref(sha) => format!("head_sha={sha}"),
                (Some(branch), _) if is_ref(branch) => format!("branch={branch}"),
                _ => return Err("branch or sha is required".into()),
            };
            let v = gh
                .get(&format!("{base}/actions/runs?{filter}&per_page=10"))
                .await?;
            let runs: Vec<Value> = v["workflow_runs"]
                .as_array()
                .map(|a| a.iter().map(run_summary).collect())
                .unwrap_or_default();
            Ok(json!({ "runs": runs }))
        }
        other => Err(format!("no github tool named {other}")),
    }
}

/// `ready` as `pr_ready` and `mr_ready` take it: true unless the call says
/// otherwise, and a boolean when it does.
pub fn ready_arg(args: &Value) -> Result<bool, String> {
    match args.get("ready") {
        None | Some(Value::Null) => Ok(true),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err("ready is true or false".into()),
    }
}

/// What `pr_ready` does with a pull request as GitHub just read it.
#[derive(Debug, PartialEq)]
enum ReadyStep {
    /// It is already in the state asked for: the answer, with no write.
    Already(Value),
    /// The mutation to send, with the pull request's node id.
    Mutate { mutation: &'static str, id: String },
}

fn ready_step(pr: &Value, ready: bool) -> Result<ReadyStep, String> {
    let n = &pr["number"];
    if pr["merged"] == true || pr["merged_at"].is_string() {
        return Err(format!(
            "pull request #{n} is merged; only an open one is marked ready or made a draft"
        ));
    }
    if pr["state"] != "open" {
        return Err(format!(
            "pull request #{n} is closed; only an open one is marked ready or made a draft"
        ));
    }
    let draft = pr["draft"] == true;
    if draft != ready {
        return Ok(ReadyStep::Already(json!({
            "number": n,
            "draft": draft,
            "head_sha": pr["head"]["sha"],
            "url": pr["html_url"],
            "already": true,
        })));
    }
    let id = pr["node_id"]
        .as_str()
        .filter(|id| is_node_id(id))
        .ok_or("github did not give the pull request's node id")?;
    Ok(ReadyStep::Mutate {
        mutation: if ready {
            READY_MUTATION
        } else {
            DRAFT_MUTATION
        },
        id: id.to_string(),
    })
}

/// How many times `pr_merge` tries again after GitHub answers that the base
/// branch moved under it, and how long it waits before the first retry (each
/// later one waits that much longer).
const MERGE_RETRIES: u32 = 3;
const MERGE_RETRY_AFTER: std::time::Duration = std::time::Duration::from_millis(500);

/// One merge at a time per repository, keyed by its API base.
fn merge_lock(base: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    type Locks =
        std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>;
    static LOCKS: std::sync::LazyLock<Locks> = std::sync::LazyLock::new(Default::default);
    LOCKS
        .lock()
        .unwrap()
        .entry(base.to_string())
        .or_default()
        .clone()
}

/// GitHub's answer when another merge moved the base during this one: nothing
/// was merged, so the same request may be sent again.
fn base_moved(error: &str) -> bool {
    error.starts_with("github answered 405") && error.contains("Base branch was modified")
}

/// How often `run_wait` reads the run while it holds.
const WAIT_EVERY: std::time::Duration = std::time::Duration::from_secs(3);

/// How long `run_dispatch` looks for the run it started when GitHub does
/// not say which it is: a dispatched run is listed within seconds.
const FIND_EVERY: std::time::Duration = std::time::Duration::from_secs(1);
const FIND_TRIES: usize = 10;

/// A `run_dispatch` call as it will be sent.
pub struct Dispatch<'a> {
    pub workflow: &'a str,
    pub branch: &'a str,
    pub inputs: serde_json::Map<String, Value>,
}

/// A `run_dispatch` call checked before anything is sent: the workflow named
/// by its file, a branch name rather than a full ref or a path, and inputs
/// that are strings, none naming production (the same rule `pipeline_run`
/// holds GitLab's variables to). Whether the workflow is one the node may
/// start is [`dispatch_refusal`]'s, which has the configuration; whether the
/// branch is a branch and not a tag is asked of GitHub when it runs.
pub fn dispatch_request(args: &Value) -> Result<Dispatch<'_>, String> {
    let workflow = args
        .get("workflow")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|w| is_workflow_file(w))
        .ok_or("workflow is required, as its file name in .github/workflows (e.g. ci.yml)")?;
    let branch = args
        .get("ref")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .ok_or("ref is required (a branch)")?;
    if !is_ref(branch)
        || branch.starts_with("refs/")
        || branch.starts_with('/')
        || branch.ends_with('/')
        || branch.contains("..")
    {
        return Err(format!("{branch} is not a branch name"));
    }
    Ok(Dispatch {
        workflow,
        branch,
        inputs: dispatch_inputs(args.get("inputs"))?,
    })
}

/// Why the node will not start this workflow, decided before the operator is
/// asked and again before it runs: anything [`dispatch_request`] refuses, or
/// a workflow `[github] dispatch` does not list for the repository. A release
/// runs as a workflow on `main`, so the ref alone cannot keep it out.
pub fn dispatch_refusal(cfg: &crate::config::Github, args: &Value) -> Option<String> {
    let repo = match repo(args) {
        Ok(repo) => repo,
        Err(e) => return Some(e),
    };
    let request = match dispatch_request(args) {
        Ok(request) => request,
        Err(e) => return Some(e),
    };
    (!cfg.may_dispatch(repo, request.workflow)).then(|| {
        format!(
            "{} is not a workflow this node starts in {repo}; the operator lists the ones \
             it may under [github] dispatch in node.toml",
            request.workflow
        )
    })
}

/// A workflow's file name in `.github/workflows`, safe in a URL path.
fn is_workflow_file(s: &str) -> bool {
    s.len() <= 255
        && !s.starts_with('.')
        && (s.ends_with(".yml") || s.ends_with(".yaml"))
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Workflow inputs as GitHub takes them: name to string. A value naming
/// production is refused here, before anything is sent: that is the deploy
/// that is run by hand, whatever the workflow.
fn dispatch_inputs(v: Option<&Value>) -> Result<serde_json::Map<String, Value>, String> {
    let Some(v) = v.filter(|v| !v.is_null()) else {
        return Ok(Default::default());
    };
    let map = v
        .as_object()
        .ok_or("inputs is an object of name to value")?;
    for (k, v) in map {
        // Anything else would be sent as its JSON text, which is not the
        // value that was approved.
        let value = v
            .as_str()
            .ok_or_else(|| format!("input {k} must be a string, e.g. \"{v}\""))?;
        if value.to_ascii_lowercase().contains("production") {
            return Err(format!(
                "input {k} names production; a production deploy is run by hand"
            ));
        }
    }
    Ok(map.clone())
}

/// The run a dispatch started, when GitHub did not say: the newest listed in
/// `now` that `before` did not have, with its URL.
fn new_run(before: &Value, now: &Value) -> Option<(i64, Value)> {
    let ids = |v: &Value| -> Vec<i64> {
        v["workflow_runs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r["id"].as_i64())
            .collect()
    };
    let seen = ids(before);
    now["workflow_runs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["id"].as_i64().is_some_and(|id| !seen.contains(&id)))
        .max_by_key(|r| r["id"].as_i64())
        .map(|r| (r["id"].as_i64().unwrap_or_default(), r["html_url"].clone()))
}

/// One run with its jobs, as `run_status` with `run_id` returns it. The jobs
/// are the latest attempt's.
async fn run_with_jobs(gh: &Client<'_>, base: &str, run: i64) -> Result<Value, String> {
    let r = gh.get(&format!("{base}/actions/runs/{run}")).await?;
    let jobs = gh
        .get(&format!("{base}/actions/runs/{run}/jobs?per_page=100"))
        .await?;
    Ok(json!({
        "run": run_summary(&r),
        "jobs": jobs_summary(&jobs),
        "more_jobs": jobs["total_count"].as_u64().unwrap_or(0)
            > jobs["jobs"].as_array().map_or(0, |j| j.len() as u64),
    }))
}

/// What one read of the run means to `run_wait`, against the state the
/// caller last saw.
fn wait_read(
    run: &Value,
    attempt: i64,
    baseline: Option<&str>,
    until: super::wait::Until,
) -> super::wait::Read {
    // Until GitHub has started the attempt asked for, the run still reads
    // as the attempt before it, finished, and its failed jobs are that
    // attempt's.
    let started = run["run"]["attempt"].as_i64().unwrap_or(0) >= attempt;
    let state = run_state_token(run);
    let changed = started && baseline.is_some_and(|b| b != state);
    let done = started && run["run"]["status"] == "completed";
    let failed = started && job_failed(run);
    let returns = super::wait::returns(until, changed, baseline.is_none(), done, failed);
    super::wait::Read {
        state,
        changed,
        done,
        returns,
    }
}

/// Whether a job in the run has failed.
fn job_failed(run: &Value) -> bool {
    run["jobs"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|j| j["conclusion"] == "failure")
}

/// What `run_wait` compares: the run's attempt, status and conclusion, and
/// each job's.
fn run_state_token(run: &Value) -> String {
    use sha2::{Digest, Sha256};
    let r = &run["run"];
    let mut seen = format!("{};{};{}", r["attempt"], r["status"], r["conclusion"]);
    let mut jobs: Vec<(i64, String)> = run["jobs"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|j| {
            (
                j["id"].as_i64().unwrap_or(0),
                format!("{}/{}", j["status"], j["conclusion"]),
            )
        })
        .collect();
    jobs.sort();
    for (id, status) in jobs {
        seen.push_str(&format!(";{id}={status}"));
    }
    hex::encode(&Sha256::digest(seen.as_bytes())[..8])
}

/// `owner/name`, validated before it reaches a URL.
fn repo(args: &Value) -> Result<&str, String> {
    args.get("repo")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|r| {
            let mut parts = r.split('/');
            let ok = |p: Option<&str>| {
                p.is_some_and(|p| {
                    !p.is_empty()
                        && !p.starts_with('.')
                        && p.chars()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                })
            };
            ok(parts.next()) && ok(parts.next()) && parts.next().is_none()
        })
        .ok_or_else(|| "repo is required, as owner/name".to_string())
}

fn positive(args: &Value, key: &str) -> Result<i64, String> {
    args.get(key)
        .and_then(Value::as_i64)
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("{key} is required"))
}

fn run_summary(r: &Value) -> Value {
    json!({
        "id": r["id"], "name": r["name"], "status": r["status"],
        "conclusion": r["conclusion"], "event": r["event"],
        "branch": r["head_branch"], "sha": r["head_sha"],
        "attempt": r["run_attempt"],
        "url": r["html_url"], "created_at": r["created_at"],
    })
}

/// A run's jobs as someone diagnosing it reads them: which failed, and in
/// which step.
fn jobs_summary(v: &Value) -> Vec<Value> {
    v["jobs"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|j| {
            let failed_steps: Vec<&Value> = j["steps"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|s| s["conclusion"] == "failure")
                .map(|s| &s["name"])
                .collect();
            json!({
                "id": j["id"], "name": j["name"], "status": j["status"],
                "conclusion": j["conclusion"], "failed_steps": failed_steps,
                "url": j["html_url"],
            })
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

fn number(args: &Value) -> Result<i64, String> {
    args.get("number")
        .and_then(Value::as_i64)
        .filter(|n| *n > 0)
        .ok_or_else(|| "number is required".to_string())
}

/// A branch name or a sha, safe to put in a query string as it is.
fn is_ref(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

/// A GraphQL node id, safe to pass as a variable and to show on an approval.
fn is_node_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '='))
}

/// Where GraphQL lives beside the REST root: `api.github.com/graphql` on
/// github.com, `<host>/api/graphql` beside an enterprise `<host>/api/v3`.
fn graphql_url(api: &str) -> String {
    match api.strip_suffix("/api/v3") {
        Some(host) => format!("{host}/api/graphql"),
        None => format!("{api}/graphql"),
    }
}

/// Each reviewer's latest review that says something about the change, and
/// the decision they add up to: any outstanding request for changes wins over
/// approvals. A comment-only review counts only when a reviewer left nothing
/// else. This is what reviewers said, not what branch protection requires.
fn review_decision(v: &Value) -> (&'static str, Vec<Value>) {
    let mut latest: Vec<(String, Value)> = Vec::new();
    for r in v.as_array().into_iter().flatten() {
        let Some(user) = r["user"]["login"].as_str() else {
            continue;
        };
        let state = r["state"].as_str().unwrap_or_default();
        let entry = json!({ "user": user, "state": state, "submitted_at": r["submitted_at"] });
        match latest.iter_mut().find(|(u, _)| u == user) {
            // Reviews come oldest first; a later comment does not undo an
            // earlier approval or request for changes.
            Some((_, seen)) if state == "COMMENTED" && seen["state"] != "COMMENTED" => {}
            Some((_, seen)) => *seen = entry,
            None => latest.push((user.to_string(), entry)),
        }
    }
    let latest: Vec<Value> = latest.into_iter().map(|(_, r)| r).collect();
    let has = |state: &str| latest.iter().any(|r| r["state"] == state);
    let decision = if has("CHANGES_REQUESTED") {
        "changes_requested"
    } else if has("APPROVED") {
        "approved"
    } else {
        "none"
    };
    (decision, latest)
}

/// Check runs, counted the way a reviewer reads them.
fn rollup(v: &Value) -> Value {
    let runs = v["check_runs"].as_array().cloned().unwrap_or_default();
    let (mut passed, mut failed, mut pending) = (0, 0, 0);
    let listed: Vec<Value> = runs
        .iter()
        .map(|r| {
            match (r["status"].as_str(), r["conclusion"].as_str()) {
                (Some("completed"), Some("success" | "neutral" | "skipped")) => passed += 1,
                (Some("completed"), _) => failed += 1,
                _ => pending += 1,
            }
            json!({ "name": r["name"], "status": r["status"], "conclusion": r["conclusion"] })
        })
        .collect();
    json!({
        "total": runs.len(),
        "passed": passed,
        "failed": failed,
        "pending": pending,
        "runs": listed,
    })
}

struct Client<'a> {
    http: &'a reqwest::Client,
    token: &'a str,
}

impl Client<'_> {
    fn request(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        // GitHub refuses a request with no User-Agent.
        self.http
            .request(method, url)
            .bearer_auth(self.token)
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28")
            .header("user-agent", crate::forge::USER_AGENT)
    }

    async fn get(&self, url: &str) -> Result<Value, String> {
        read(self.request(reqwest::Method::GET, url).send().await).await
    }

    async fn post(&self, url: &str, body: &Value) -> Result<Value, String> {
        read(
            self.request(reqwest::Method::POST, url)
                .json(body)
                .send()
                .await,
        )
        .await
    }

    /// A GraphQL call. GraphQL answers 200 with `errors` for most failures, so
    /// those are read as the failure they are.
    async fn graphql(&self, url: &str, query: &str, variables: Value) -> Result<Value, String> {
        let v = self
            .post(url, &json!({ "query": query, "variables": variables }))
            .await?;
        if let Some(errors) = v["errors"].as_array().filter(|e| !e.is_empty()) {
            let messages: Vec<&str> = errors
                .iter()
                .filter_map(|e| e["message"].as_str())
                .collect();
            return Err(format!("github refused: {}", messages.join("; ")));
        }
        Ok(v)
    }

    /// A body that is text, not JSON: a job's log. GitHub answers with a
    /// redirect to short-lived storage that needs no token, so the redirect
    /// is followed here, without one — whatever host it names.
    async fn text(&self, url: &str) -> Result<String, String> {
        static NO_REDIRECT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
        let direct = NO_REDIRECT.get_or_init(|| {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("a client with no redirect policy builds")
        });
        let first = direct
            .get(url)
            .bearer_auth(self.token)
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28")
            .header("user-agent", "tracon")
            .send()
            .await
            .map_err(|e| format!("github: {e}"))?;
        let res = if first.status().is_redirection() {
            let location = first
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|l| l.to_str().ok())
                .ok_or("github redirected the log to nowhere")?;
            let target = first
                .url()
                .join(location)
                .map_err(|e| format!("github redirected the log somewhere unreadable: {e}"))?;
            self.http
                .get(target)
                .header("user-agent", "tracon")
                .send()
                .await
                .map_err(|e| format!("github: {e}"))?
        } else {
            first
        };
        let status = res.status();
        let body = res.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!("github answered {status} for the log"));
        }
        Ok(body)
    }

    /// A POST with no body whose success is the status alone (201 or 204).
    async fn post_empty(&self, url: &str) -> Result<(), String> {
        let res = self
            .request(reqwest::Method::POST, url)
            .send()
            .await
            .map_err(|e| format!("github: {e}"))?;
        let status = res.status();
        if !status.is_success() {
            let v: Value = res.json().await.unwrap_or(Value::Null);
            return Err(format!("github answered {status}: {}", v["message"]));
        }
        Ok(())
    }

    async fn put(&self, url: &str, body: &Value) -> Result<Value, String> {
        read(
            self.request(reqwest::Method::PUT, url)
                .json(body)
                .send()
                .await,
        )
        .await
    }
}

async fn read(sent: reqwest::Result<reqwest::Response>) -> Result<Value, String> {
    let res = sent.map_err(|e| format!("github: {e}"))?;
    let status = res.status();
    let v: Value = res.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(format!("github answered {status}: {}", v["message"]));
    }
    Ok(v)
}

fn mutation_outcome_error(error: String) -> String {
    if error.starts_with("github:") || error.starts_with("github answered 5") {
        format!("mutation-outcome-unknown: {error}")
    } else {
        error
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_moved_base_is_retried() {
        assert!(base_moved(
            "github answered 405 Method Not Allowed: \"Base branch was modified. Review and try the merge again.\""
        ));
        assert!(!base_moved(
            "github answered 405 Method Not Allowed: \"Pull Request is not mergeable\""
        ));
        assert!(!base_moved(
            "github answered 409 Conflict: \"Head branch was modified\""
        ));
    }

    #[test]
    fn ready_marks_a_draft_ready_and_leaves_one_already_there_alone() {
        let pr = |draft: bool| {
            json!({ "number": 7, "state": "open", "merged": false, "draft": draft,
                    "node_id": "PR_kwDO", "head": { "sha": "abc123" },
                    "html_url": "https://github.test/o/n/pull/7" })
        };
        assert_eq!(
            ready_step(&pr(true), true).unwrap(),
            ReadyStep::Mutate {
                mutation: READY_MUTATION,
                id: "PR_kwDO".into()
            }
        );
        assert_eq!(
            ready_step(&pr(false), false).unwrap(),
            ReadyStep::Mutate {
                mutation: DRAFT_MUTATION,
                id: "PR_kwDO".into()
            }
        );
        assert!(READY_MUTATION.contains("markPullRequestReadyForReview"));
        assert!(DRAFT_MUTATION.contains("convertPullRequestToDraft"));
        let ReadyStep::Already(out) = ready_step(&pr(false), true).unwrap() else {
            panic!("a ready pull request is written to again");
        };
        assert_eq!(out["already"], true);
        assert_eq!(out["draft"], false);
        assert_eq!(out["head_sha"], "abc123");
        assert!(matches!(
            ready_step(&pr(true), false).unwrap(),
            ReadyStep::Already(_)
        ));
    }

    #[test]
    fn ready_refuses_a_closed_or_merged_pull_request_and_a_ready_that_is_not_a_boolean() {
        let closed = json!({ "number": 7, "state": "closed", "merged": false, "draft": true });
        assert!(ready_step(&closed, true).unwrap_err().contains("is closed"));
        let merged = json!({ "number": 7, "state": "closed", "merged": true, "draft": false });
        assert!(ready_step(&merged, false)
            .unwrap_err()
            .contains("is merged"));
        assert_eq!(ready_arg(&json!({})), Ok(true));
        assert_eq!(ready_arg(&json!({ "ready": false })), Ok(false));
        assert!(ready_arg(&json!({ "ready": "false" })).is_err());
    }

    #[test]
    fn a_repo_is_owner_and_name_and_nothing_else() {
        assert_eq!(
            repo(&json!({ "repo": "cosmicspork/tracon" })).unwrap(),
            "cosmicspork/tracon"
        );
        for bad in ["tracon", "a/b/c", "../x", "a/.git", "a b/c", ""] {
            assert!(repo(&json!({ "repo": bad })).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_request_for_changes_outweighs_approvals_and_a_later_comment_undoes_neither() {
        let (decision, latest) = review_decision(&json!([
            { "user": { "login": "a" }, "state": "APPROVED" },
            { "user": { "login": "b" }, "state": "CHANGES_REQUESTED" },
            { "user": { "login": "b" }, "state": "COMMENTED" },
            { "user": { "login": "c" }, "state": "COMMENTED" },
        ]));
        assert_eq!(decision, "changes_requested");
        assert_eq!(latest.len(), 3);
        assert_eq!(latest[1]["state"], "CHANGES_REQUESTED");
        let (decision, _) = review_decision(&json!([
            { "user": { "login": "b" }, "state": "CHANGES_REQUESTED" },
            { "user": { "login": "b" }, "state": "APPROVED" },
        ]));
        assert_eq!(decision, "approved");
        assert_eq!(review_decision(&Value::Null).0, "none");
    }

    #[test]
    fn graphql_sits_beside_the_rest_root() {
        assert_eq!(
            graphql_url("https://api.github.com"),
            "https://api.github.com/graphql"
        );
        assert_eq!(
            graphql_url("https://ghe.example/api/v3"),
            "https://ghe.example/api/graphql"
        );
    }

    #[test]
    fn a_runs_jobs_say_which_step_failed() {
        let jobs = jobs_summary(&json!({ "jobs": [
            { "id": 1, "name": "test", "status": "completed", "conclusion": "failure",
              "steps": [
                { "name": "checkout", "conclusion": "success" },
                { "name": "cargo test", "conclusion": "failure" },
              ] },
            { "id": 2, "name": "lint", "status": "completed", "conclusion": "success", "steps": [] },
        ] }));
        assert_eq!(jobs[0]["failed_steps"], json!(["cargo test"]));
        assert_eq!(jobs[1]["failed_steps"], json!([]));
        assert_eq!(tail("abcdef", 3), ("def", true));
        assert_eq!(tail("ab", 3), ("ab", false));
    }

    #[test]
    fn a_dispatch_is_a_listed_workflow_on_a_branch_with_string_inputs() {
        let cfg = crate::config::Github {
            dispatch: [("o/n".to_string(), vec!["renovate.yaml".to_string()])].into(),
        };
        let ok = json!({ "repo": "o/n", "workflow": "renovate.yaml", "ref": "main",
                         "inputs": { "LOG_LEVEL": "debug", "N": "2" } });
        assert_eq!(dispatch_refusal(&cfg, &ok), None);
        let d = dispatch_request(&ok).unwrap();
        assert_eq!(
            (d.workflow, d.branch, d.inputs.len()),
            ("renovate.yaml", "main", 2)
        );

        // Deny by default: a workflow, or a repository, the file does not list.
        for (repo, workflow) in [("o/n", "release.yml"), ("o/other", "renovate.yaml")] {
            let e = dispatch_refusal(
                &cfg,
                &json!({ "repo": repo, "workflow": workflow, "ref": "main" }),
            )
            .unwrap();
            assert!(e.contains("[github] dispatch"), "{e}");
        }
        let e = dispatch_refusal(
            &Default::default(),
            &json!({ "repo": "o/n", "workflow": "renovate.yaml", "ref": "main" }),
        );
        assert!(e.is_some());

        for workflow in [
            "",
            "../ci.yml",
            ".github/workflows/ci.yml",
            "ci",
            "1234",
            ".x.yml",
        ] {
            let args = json!({ "repo": "o/n", "workflow": workflow, "ref": "main" });
            assert!(dispatch_request(&args).is_err(), "{workflow}");
        }
        for branch in ["", "refs/tags/v1", "a..b", "/main", "main/", "a b", "x?y=1"] {
            let args = json!({ "repo": "o/n", "workflow": "ci.yml", "ref": branch });
            assert!(dispatch_request(&args).is_err(), "{branch}");
        }
        assert!(
            dispatch_request(&json!({ "repo": "o/n", "workflow": "ci.yml", "ref": "feat/x" }))
                .is_ok()
        );
    }

    #[test]
    fn a_dispatch_input_is_a_string_and_never_names_production() {
        let e = dispatch_inputs(Some(&json!({ "TARGET": "Production" }))).unwrap_err();
        assert!(e.contains("TARGET"), "{e}");
        for value in [json!(2), json!(true), json!({ "a": 1 }), json!(["x"])] {
            let e = dispatch_inputs(Some(&json!({ "N": value }))).unwrap_err();
            assert!(e.contains("N must be a string"), "{e}");
        }
        assert!(dispatch_inputs(None).unwrap().is_empty());
        assert!(dispatch_inputs(Some(&json!("x"))).is_err());
    }

    #[test]
    fn the_run_a_dispatch_started_is_the_newest_one_not_there_before() {
        let before = json!({ "workflow_runs": [{ "id": 10 }, { "id": 9 }] });
        assert_eq!(new_run(&before, &before), None);
        let now = json!({ "workflow_runs": [
            { "id": 12, "html_url": "u12" }, { "id": 11, "html_url": "u11" }, { "id": 10 },
        ] });
        assert_eq!(new_run(&before, &now), Some((12, json!("u12"))));
    }

    #[test]
    fn checks_are_counted_as_a_reviewer_reads_them() {
        let v = rollup(&json!({ "check_runs": [
            { "name": "test", "status": "completed", "conclusion": "success" },
            { "name": "skip", "status": "completed", "conclusion": "skipped" },
            { "name": "lint", "status": "completed", "conclusion": "failure" },
            { "name": "e2e", "status": "in_progress", "conclusion": null }
        ] }));
        assert_eq!(v["total"], 4);
        assert_eq!(v["passed"], 2);
        assert_eq!(v["failed"], 1);
        assert_eq!(v["pending"], 1);
    }

    fn run_read(attempt: i64, status: &str, jobs: &[(i64, &str, Option<&str>)]) -> Value {
        let jobs: Vec<Value> = jobs
            .iter()
            .map(|(id, status, conclusion)| json!({ "id": id, "status": status, "conclusion": conclusion }))
            .collect();
        json!({ "run": { "attempt": attempt, "status": status }, "jobs": jobs })
    }

    /// Feeds a scripted run of reads through `run_wait`'s decision, carrying
    /// the state forward as one call does, and says at which read it returns.
    fn returns_at(
        reads: &[Value],
        attempt: i64,
        until: super::super::wait::Until,
    ) -> Option<usize> {
        let mut baseline: Option<String> = None;
        for (i, now) in reads.iter().enumerate() {
            let read = wait_read(now, attempt, baseline.as_deref(), until);
            if read.returns {
                return Some(i);
            }
            baseline.get_or_insert(read.state);
        }
        None
    }

    #[test]
    fn a_run_wait_until_finished_or_failure_holds_through_job_moves() {
        use super::super::wait::Until;
        let reads = [
            run_read(
                1,
                "in_progress",
                &[(1, "queued", None), (2, "queued", None)],
            ),
            run_read(
                1,
                "in_progress",
                &[(1, "in_progress", None), (2, "queued", None)],
            ),
            run_read(
                1,
                "in_progress",
                &[(1, "completed", Some("failure")), (2, "in_progress", None)],
            ),
            run_read(
                1,
                "completed",
                &[
                    (1, "completed", Some("failure")),
                    (2, "completed", Some("success")),
                ],
            ),
        ];
        assert_eq!(returns_at(&reads, 0, Until::Change), Some(1));
        assert_eq!(returns_at(&reads, 0, Until::Finished), Some(3));
        assert_eq!(returns_at(&reads, 0, Until::Failure), Some(2));
    }

    #[test]
    fn a_run_wait_until_failure_does_not_take_the_last_attempts_failure_for_this_ones() {
        use super::super::wait::Until;
        let reads = [
            // GitHub has not begun attempt 2: the run is still attempt 1, failed.
            run_read(1, "completed", &[(1, "completed", Some("failure"))]),
            run_read(2, "queued", &[(3, "queued", None)]),
            run_read(2, "in_progress", &[(3, "in_progress", None)]),
            run_read(2, "completed", &[(3, "completed", Some("success"))]),
        ];
        assert_eq!(returns_at(&reads, 2, Until::Failure), Some(3));
        assert_eq!(returns_at(&reads, 2, Until::Finished), Some(3));
    }
}
