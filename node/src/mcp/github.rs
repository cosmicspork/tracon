//! GitHub, as narrow tools: a pull request's state with its checks and
//! reviews, its review threads, one comment on it, one reply to a thread, the
//! open pull request for a branch, and the Actions runs for a branch or a
//! commit. Opening a
//! pull request is the review path (`review::publish`); merging is
//! `pr_merge`, which runs only with current scoped authority for the exact
//! head SHA. Marking ready is not a tool. The token never leaves the node.

use serde_json::{json, Value};

use crate::{broker::SharedBroker, forge::Forge, mcp::CallContext};

pub const CREDENTIAL: &str = "gh";
pub const PR_STATUS: &str = "pr_status";
pub const PR_COMMENT: &str = "pr_comment";
pub const RUN_STATUS: &str = "run_status";
pub const PR_MERGE: &str = "pr_merge";
pub const PR_THREADS: &str = "pr_threads";
pub const PR_REPLY: &str = "pr_reply";
pub const PR_FOR_BRANCH: &str = "pr_for_branch";

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
                    "repo": { "type": "string", "description": "owner/name" },
                    "number": { "type": "integer" },
                },
                "required": ["repo", "number"],
            },
        }),
        json!({
            "name": PR_COMMENT,
            "description": "Post one comment on a GitHub pull request. The operator is asked \
                            before it posts: the call returns `awaiting_operator` with an `approval_id` at once, and approval_status reports the outcome.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": "owner/name" },
                    "number": { "type": "integer" },
                    "body": { "type": "string" },
                },
                "required": ["repo", "number", "body"],
            },
        }),
        json!({
            "name": RUN_STATUS,
            "description": "The latest GitHub Actions runs for a branch or a commit.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": "owner/name" },
                    "branch": { "type": "string" },
                    "sha": { "type": "string" },
                },
                "required": ["repo"],
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
                    "repo": { "type": "string", "description": "owner/name" },
                    "number": { "type": "integer" },
                    "unresolved_only": { "type": "boolean", "description": "Leave out resolved threads." },
                },
                "required": ["repo", "number"],
            },
        }),
        json!({
            "name": PR_REPLY,
            "description": "Reply to one review thread on a GitHub pull request, and optionally \
                            resolve it. The operator is asked before it posts: the call returns `awaiting_operator` with an `approval_id` at once, and approval_status reports the outcome.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": "owner/name" },
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
                    "repo": { "type": "string", "description": "owner/name" },
                    "branch": { "type": "string" },
                },
                "required": ["repo", "branch"],
            },
        }),
        json!({
            "name": PR_MERGE,
            "description": "Merge one GitHub pull request at the exact head SHA. This only runs with current scoped authority.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": "owner/name" },
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
        PR_COMMENT => {
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
        PR_MERGE => {
            let n = number(args)?;
            let head_sha = args
                .get("head_sha")
                .and_then(Value::as_str)
                .filter(|s| s.len() >= 7)
                .ok_or("head_sha is required")?;
            let pr = gh.get(&format!("{base}/pulls/{n}")).await?;
            if pr["head"]["sha"].as_str() != Some(head_sha) {
                return Err("pull request head changed since the authorized revision".into());
            }
            let method = args
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("squash");
            if !matches!(method, "merge" | "squash" | "rebase") {
                return Err("method must be merge, squash, or rebase".into());
            }
            if let Some(recheck) = before_mutation {
                recheck()?;
            }
            let v = gh
                .put(
                    &format!("{base}/pulls/{n}/merge"),
                    &json!({ "sha": head_sha, "merge_method": method }),
                )
                .await
                .map_err(mutation_outcome_error)?;
            Ok(json!({ "merged": v["merged"], "sha": v["sha"], "message": v["message"] }))
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
                .map(|a| {
                    a.iter()
                        .map(|r| {
                            json!({
                                "id": r["id"], "name": r["name"], "status": r["status"],
                                "conclusion": r["conclusion"], "event": r["event"],
                                "branch": r["head_branch"], "sha": r["head_sha"],
                                "url": r["html_url"], "created_at": r["created_at"],
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(json!({ "runs": runs }))
        }
        other => Err(format!("no github tool named {other}")),
    }
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
            .header("user-agent", "tracon")
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
}
