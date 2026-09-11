//! GitHub, as narrow tools: a pull request's state with its checks, one
//! comment on it, and the Actions runs for a branch or a commit. Opening a
//! pull request is the review path (`review::publish`); merging and marking
//! ready are not tools at all. The token never leaves the node.

use serde_json::{json, Value};

use crate::{broker::SharedBroker, forge::Forge, mcp::CallContext};

pub const CREDENTIAL: &str = "gh";
pub const PR_STATUS: &str = "pr_status";
pub const PR_COMMENT: &str = "pr_comment";
pub const RUN_STATUS: &str = "run_status";
pub const PR_MERGE: &str = "pr_merge";

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": PR_STATUS,
            "description": "The state of a GitHub pull request: open/closed/merged, draft, \
                            mergeability, and its checks rolled up (passed, failed, pending).",
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
                            before it posts.",
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
            }))
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
