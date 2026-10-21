//! The GitHub tools against a stub of its API: the token and the User-Agent
//! GitHub requires reach only the stub, a comment waits on the operator, and
//! nothing in the surface can merge.

#[path = "support/mod.rs"]
mod support;
use support::state;

use std::sync::{Arc, Mutex};

use axum::{http::HeaderMap, routing::any, Json, Router};
use serde_json::{json, Value};
use tracon::mcp::{CallContext, Tools};

/// Method, path and query, authorization, user-agent.
type Seen = Arc<Mutex<Vec<(String, String, String, String)>>>;
/// The JSON bodies of GraphQL calls, in order.
type Bodies = Arc<Mutex<Vec<Value>>>;

async fn stub(
    axum::extract::State((seen, bodies)): axum::extract::State<(Seen, Bodies)>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: String,
) -> (axum::http::StatusCode, Json<Value>) {
    let header = |h: &str| {
        headers
            .get(h)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let target = match uri.query() {
        Some(q) => format!("{}?{q}", uri.path()),
        None => uri.path().to_string(),
    };
    seen.lock().unwrap().push((
        method.to_string(),
        target,
        header("authorization"),
        header("user-agent"),
    ));
    let path = uri.path();
    let ok = |v: Value| (axum::http::StatusCode::OK, Json(v));
    if path.ends_with("/dispatches") {
        bodies
            .lock()
            .unwrap()
            .push(serde_json::from_str(&body).unwrap_or(Value::Null));
    }
    if (method.as_str(), path) == ("POST", "/graphql") {
        let sent: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        bodies.lock().unwrap().push(sent.clone());
        let query = sent["query"].as_str().unwrap_or_default();
        let pull = |field: &str, draft: bool| {
            json!({ "data": { field: { "pullRequest": {
                "isDraft": draft, "headRefOid": "def456",
                "url": "https://github.test/owner/name/pull/8" } } } })
        };
        return ok(if sent["variables"]["id"] == "PR_kwDOstuck" {
            // GitHub answers the mutation and the pull request stays a draft.
            pull("markPullRequestReadyForReview", true)
        } else if query.contains("markPullRequestReadyForReview") {
            pull("markPullRequestReadyForReview", false)
        } else if query.contains("convertPullRequestToDraft") {
            pull("convertPullRequestToDraft", true)
        } else if query.contains("addPullRequestReviewThreadReply") {
            json!({ "data": { "addPullRequestReviewThreadReply": {
                "comment": { "url": "https://github.test/owner/name/pull/7#r9" } } } })
        } else if query.contains("resolveReviewThread") {
            json!({ "data": { "resolveReviewThread": { "thread": { "isResolved": true } } } })
        } else if sent["variables"]["number"] == 404 {
            json!({ "errors": [{ "message": "Could not resolve to a PullRequest" }] })
        } else {
            json!({ "data": { "repository": { "pullRequest": { "reviewThreads": {
                "pageInfo": { "hasNextPage": false },
                "nodes": [
                    { "id": "PRRT_open", "isResolved": false, "isOutdated": false,
                      "path": "src/a.rs", "line": 12, "originalLine": 10,
                      "comments": { "nodes": [
                        { "author": { "login": "rev" }, "body": "rename this",
                          "createdAt": "t", "url": "https://github.test/r1" }] } },
                    { "id": "PRRT_done", "isResolved": true, "isOutdated": true,
                      "path": "src/b.rs", "line": null, "originalLine": 3,
                      "comments": { "nodes": [] } }
                ] } } } } })
        });
    }
    match (method.as_str(), path) {
        // A draft until a mutation says otherwise: the last one sent decides.
        ("GET", "/repos/owner/name/pulls/8") => {
            let draft = bodies
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find_map(|b| {
                    let q = b["query"].as_str().unwrap_or_default();
                    if q.contains("markPullRequestReadyForReview") {
                        Some(false)
                    } else if q.contains("convertPullRequestToDraft") {
                        Some(true)
                    } else {
                        None
                    }
                })
                .unwrap_or(true);
            ok(json!({
                "number": 8, "state": "open", "draft": draft, "merged": false,
                "node_id": "PR_kwDOeight", "head": { "ref": "feat/eight", "sha": "def456" },
                "html_url": "https://github.test/owner/name/pull/8"
            }))
        }
        ("GET", "/repos/owner/name/pulls/12") => ok(json!({
            "number": 12, "state": "open", "draft": true, "merged": false,
            "node_id": "PR_kwDOstuck", "head": { "sha": "def456" }
        })),
        ("GET", "/repos/owner/name/pulls/9") => ok(json!({
            "number": 9, "state": "closed", "draft": true, "merged": true,
            "node_id": "PR_kwDOnine", "head": { "sha": "987fed" }
        })),
        ("GET", "/repos/owner/name/pulls/7") => ok(json!({
            "number": 7, "title": "Add thing", "state": "open", "draft": false,
            "merged": false, "mergeable": true, "mergeable_state": "clean",
            "head": { "ref": "feat/thing", "sha": "abc123" }, "base": { "ref": "main" },
            "comments": 2, "html_url": "https://github.test/owner/name/pull/7"
        })),
        ("GET", "/repos/owner/name/commits/abc123/check-runs") => ok(json!({ "check_runs": [
            { "name": "test", "status": "completed", "conclusion": "success" },
            { "name": "lint", "status": "completed", "conclusion": "failure" },
            { "name": "e2e", "status": "in_progress", "conclusion": null }
        ] })),
        ("GET", "/repos/owner/name/pulls/7/reviews") => ok(json!([
            { "user": { "login": "a" }, "state": "APPROVED", "submitted_at": "t1" },
            { "user": { "login": "b" }, "state": "CHANGES_REQUESTED", "submitted_at": "t2" },
            { "user": { "login": "b" }, "state": "COMMENTED", "submitted_at": "t3" }
        ])),
        ("GET", "/repos/owner/name/issues/7/comments") => ok(json!([
            { "id": 4, "user": { "login": "a" }, "body": "general note",
              "created_at": "t", "html_url": "https://github.test/c4" }
        ])),
        ("GET", "/repos/owner/name/pulls") => ok(json!([
            { "number": 7, "title": "Add thing", "draft": false,
              "base": { "ref": "main" }, "html_url": "https://github.test/owner/name/pull/7" }
        ])),
        ("POST", "/repos/owner/name/issues/7/comments") => (
            axum::http::StatusCode::CREATED,
            Json(json!({ "id": 5, "html_url": "https://github.test/owner/name/pull/7#c5" })),
        ),
        ("GET", "/repos/owner/name/actions/runs") => ok(json!({ "workflow_runs": [{
            "id": 900, "name": "ci", "status": "completed", "conclusion": "success",
            "event": "push", "head_branch": "main", "head_sha": "abc123",
            "html_url": "https://github.test/owner/name/actions/runs/900",
            "created_at": "2026-09-10T00:00:00Z"
        }] })),
        ("GET", "/repos/owner/name/actions/runs/900") => ok(json!({
            "id": 900, "name": "ci", "status": "completed", "conclusion": "failure",
            "event": "push", "head_branch": "main", "head_sha": "abc123", "run_attempt": 1,
            "html_url": "https://github.test/owner/name/actions/runs/900",
            "created_at": "2026-09-10T00:00:00Z"
        })),
        ("GET", "/repos/owner/name/actions/runs/900/jobs") => {
            ok(json!({ "total_count": 2, "jobs": [
            { "id": 31, "name": "test", "status": "completed", "conclusion": "failure",
              "html_url": "https://github.test/j31", "steps": [
                { "name": "checkout", "conclusion": "success" },
                { "name": "cargo test", "conclusion": "failure" } ] },
            { "id": 32, "name": "lint", "status": "completed", "conclusion": "success",
              "html_url": "https://github.test/j32", "steps": [] }
        ] }))
        }
        ("POST", "/repos/owner/name/actions/runs/900/rerun-failed-jobs") => {
            (axum::http::StatusCode::CREATED, Json(json!({})))
        }
        ("GET", "/repos/owner/name/git/ref/tags/v1") => ok(json!({ "ref": "refs/tags/v1" })),
        ("GET", "/repos/owner/name/branches/main") => ok(json!({ "name": "main" })),
        ("GET", "/repos/owner/name/actions/workflows/renovate.yaml/runs") => {
            ok(json!({ "workflow_runs": [{ "id": 940 }] }))
        }
        ("POST", "/repos/owner/name/actions/workflows/renovate.yaml/dispatches") => ok(json!({
            "workflow_run_id": 950,
            "run_url": "https://api.github.test/repos/owner/name/actions/runs/950",
            "html_url": "https://github.test/owner/name/actions/runs/950",
        })),
        // An older GitHub that does not say which run it started: the run
        // appears in the list after the dispatch.
        ("GET", "/repos/owner/name/actions/workflows/quiet.yml/runs") => {
            let dispatched = seen
                .lock()
                .unwrap()
                .iter()
                .any(|(m, p, ..)| m == "POST" && p.ends_with("quiet.yml/dispatches"));
            ok(if dispatched {
                json!({ "workflow_runs": [
                    { "id": 961, "html_url": "https://github.test/owner/name/actions/runs/961" },
                    { "id": 940 },
                ] })
            } else {
                json!({ "workflow_runs": [{ "id": 940 }] })
            })
        }
        ("POST", "/repos/owner/name/actions/workflows/quiet.yml/dispatches") => {
            (axum::http::StatusCode::NO_CONTENT, Json(Value::Null))
        }
        _ => (
            axum::http::StatusCode::NOT_FOUND,
            Json(json!({ "message": "Not Found" })),
        ),
    }
}

/// What reached the log storage GitHub redirects to: its authorization header.
static STORAGE_AUTH: Mutex<Vec<String>> = Mutex::new(Vec::new());

async fn job_logs() -> axum::response::Redirect {
    axum::response::Redirect::temporary("/storage/job-31.log")
}

async fn storage(headers: HeaderMap) -> String {
    STORAGE_AUTH.lock().unwrap().push(
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string(),
    );
    format!(
        "{}error: test failed, to rerun pass `-p node`\n",
        "noise\n".repeat(5000)
    )
}

async fn rig() -> (Tools, Seen) {
    let (tools, seen, _) = rig_with_bodies().await;
    (tools, seen)
}

async fn rig_with_bodies() -> (Tools, Seen, Bodies) {
    let seen = Seen::default();
    let bodies = Bodies::default();
    let app = Router::new()
        .route(
            "/repos/owner/name/actions/jobs/31/logs",
            axum::routing::get(job_logs),
        )
        .route("/storage/job-31.log", axum::routing::get(storage))
        .route("/{*path}", any(stub))
        .with_state((seen.clone(), bodies.clone()));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });
    let creds = format!(
        r#"
        [credentials.gh]
        channels = ["work"]
        [credentials.gh.env]
        GITHUB_API = "{base}"
        GH_TOKEN = "gh-secret"
        "#
    );
    let tools = Tools {
        broker: Arc::new(toml::from_str(&creds).unwrap()),
        cfg: Arc::new(tracon::config::Config::default()),
        policy: tracon::policy::Policy::shipped_shared(),
        http: reqwest::Client::new(),
        session: Default::default(),
    };
    (tools, seen, bodies)
}

fn allowing(names: &str) -> Arc<parking_lot::RwLock<tracon::policy::Policy>> {
    Arc::new(parking_lot::RwLock::new(
        toml::from_str(&format!(
            r#"
        version = 9
        [[rule]]
        id = "test-allow"
        verdict = "allow"
        reason = "Under test."
        kinds = ["tool"]
        matches = [{names}]
        "#
        ))
        .unwrap(),
    ))
}

fn ctx(channel: &str) -> CallContext {
    CallContext::session("s", channel, "n1")
}

async fn call(t: &Tools, c: &CallContext, name: &str, args: Value) -> (bool, Value) {
    let res = t
        .handle(
            c,
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}}),
        )
        .await
        .unwrap();
    let text = res["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    (
        res["result"]["isError"] == true,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

#[tokio::test]
async fn the_github_tools_are_offered_to_the_bound_channel() {
    state::isolate();
    let (t, _) = rig().await;
    let names: Vec<String> = t
        .list("work", "n1")
        .iter()
        .map(|d| d["name"].as_str().unwrap().to_string())
        .collect();
    for n in [
        "pr_status",
        "pr_comment",
        "github_issue_comment",
        "run_status",
    ] {
        assert!(names.contains(&n.to_string()), "{n} missing from {names:?}");
    }
    assert!(t.list("personal", "n1").is_empty());
}

#[tokio::test]
async fn pr_status_rolls_up_the_checks_on_the_head_commit() {
    state::isolate();
    let (mut t, seen) = rig().await;
    t.policy = allowing(r#""pr_status""#);
    let (err, v) = call(
        &t,
        &ctx("work"),
        "pr_status",
        json!({ "repo": "owner/name", "number": 7 }),
    )
    .await;
    assert!(!err, "{v}");
    assert_eq!(v["state"], "open");
    assert_eq!(v["head"], "feat/thing");
    assert_eq!(v["checks"]["passed"], 1);
    assert_eq!(v["checks"]["failed"], 1);
    assert_eq!(v["checks"]["pending"], 1);
    assert_eq!(v["review_decision"], "changes_requested");
    assert_eq!(v["reviews"].as_array().unwrap().len(), 2);
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 3);
    assert!(seen
        .iter()
        .all(|(_, _, auth, _)| auth == "Bearer gh-secret"));
    assert!(seen.iter().all(|(_, _, _, ua)| !ua.is_empty()));
}

#[tokio::test]
async fn a_comment_waits_on_the_operator_and_then_posts_once() {
    state::isolate();
    let (mut t, seen) = rig().await;
    let c = ctx("work");
    let args = json!({ "repo": "owner/name", "number": 7, "body": "looks fine" });
    let (err, v) = call(&t, &c, "pr_comment", args.clone()).await;
    assert!(err);
    assert!(v.as_str().unwrap_or_default().contains("approval"), "{v}");
    assert!(seen.lock().unwrap().is_empty(), "nothing reached GitHub");

    t.policy = allowing(r#""pr_comment""#);
    let (err, v) = call(&t, &c, "pr_comment", args).await;
    assert!(!err, "{v}");
    assert_eq!(v["id"], 5);
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, "POST");
    assert_eq!(seen[0].1, "/repos/owner/name/issues/7/comments");
}

/// An issue had no comment tool of its own, so an agent had to guess that
/// `pr_comment` takes an issue number too (#593).
#[tokio::test]
async fn an_issue_comment_waits_on_the_operator_and_then_posts_on_the_issue() {
    state::isolate();
    let (mut t, seen) = rig().await;
    let c = ctx("work");
    let args = json!({ "repo": "owner/name", "number": 7, "body": "blocked on #592" });
    let (err, v) = call(&t, &c, "github_issue_comment", args.clone()).await;
    assert!(err);
    assert!(v.as_str().unwrap_or_default().contains("approval"), "{v}");
    assert!(seen.lock().unwrap().is_empty(), "nothing reached GitHub");

    t.policy = allowing(r#""github_issue_comment""#);
    let (err, v) = call(&t, &c, "github_issue_comment", args).await;
    assert!(!err, "{v}");
    assert_eq!(v["id"], 5);
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, "POST");
    assert_eq!(seen[0].1, "/repos/owner/name/issues/7/comments");
}

/// Marking a draft ready was `gh pr ready` on the host, outside the broker
/// and its log (#600). It is asked like any other write, and turning it back
/// into a draft goes the same way.
#[tokio::test]
async fn pr_ready_waits_on_the_operator_then_marks_the_draft_ready_and_back() {
    state::isolate();
    let (mut t, seen, bodies) = rig_with_bodies().await;
    let c = ctx("work");
    let args = json!({ "repo": "owner/name", "number": 8 });
    let (err, v) = call(&t, &c, "pr_ready", args.clone()).await;
    assert!(err);
    assert!(v.as_str().unwrap_or_default().contains("approval"), "{v}");
    assert!(seen.lock().unwrap().is_empty(), "nothing reached GitHub");

    t.policy = allowing(r#""pr_ready""#);
    let (err, v) = call(&t, &c, "pr_ready", args.clone()).await;
    assert!(!err, "{v}");
    assert_eq!(v["draft"], false);
    assert_eq!(v["head_sha"], "def456");
    assert_eq!(v["already"], false);
    let sent = bodies.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert!(sent[0]["query"]
        .as_str()
        .unwrap()
        .contains("markPullRequestReadyForReview"));
    assert_eq!(sent[0]["variables"]["id"], "PR_kwDOeight");

    // Ready already: answered as it stands, with no second write.
    let (err, v) = call(&t, &c, "pr_ready", args).await;
    assert!(!err, "{v}");
    assert_eq!(v["already"], true);
    assert_eq!(v["draft"], false);
    assert_eq!(bodies.lock().unwrap().len(), 1);

    let back = json!({ "repo": "owner/name", "number": 8, "ready": false });
    let (err, v) = call(&t, &c, "pr_ready", back.clone()).await;
    assert!(!err, "{v}");
    assert_eq!(v["draft"], true);
    assert_eq!(v["already"], false);
    assert!(bodies.lock().unwrap()[1]["query"]
        .as_str()
        .unwrap()
        .contains("convertPullRequestToDraft"));
    let (_, v) = call(&t, &c, "pr_ready", back).await;
    assert_eq!(v["already"], true, "{v}");
    assert_eq!(bodies.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn pr_ready_refuses_a_merged_pull_request_without_writing() {
    state::isolate();
    let (mut t, _, bodies) = rig_with_bodies().await;
    t.policy = allowing(r#""pr_ready""#);
    let (err, v) = call(
        &t,
        &ctx("work"),
        "pr_ready",
        json!({ "repo": "owner/name", "number": 9 }),
    )
    .await;
    assert!(err);
    assert!(v.as_str().unwrap_or_default().contains("is merged"), "{v}");
    assert!(bodies.lock().unwrap().is_empty());
}

/// The card says which way the call turns the pull request, so a `ready` it
/// cannot read is refused before the operator is asked about it.
#[tokio::test]
async fn pr_ready_refuses_a_ready_that_is_not_a_boolean_before_asking() {
    state::isolate();
    let (t, seen, _) = rig_with_bodies().await;
    let (err, v) = call(
        &t,
        &ctx("work"),
        "pr_ready",
        json!({ "repo": "owner/name", "number": 8, "ready": "false" }),
    )
    .await;
    assert!(err);
    let said = v.as_str().unwrap_or_default();
    assert!(said.contains("ready is true or false"), "{v}");
    assert!(!said.contains("approval"), "{v}");
    assert!(seen.lock().unwrap().is_empty(), "nothing reached GitHub");
}

#[tokio::test]
async fn pr_ready_is_an_error_when_github_still_reads_a_draft_after_the_mutation() {
    state::isolate();
    let (mut t, _, bodies) = rig_with_bodies().await;
    t.policy = allowing(r#""pr_ready""#);
    let (err, v) = call(
        &t,
        &ctx("work"),
        "pr_ready",
        json!({ "repo": "owner/name", "number": 12 }),
    )
    .await;
    assert!(err);
    assert!(
        v.as_str()
            .unwrap_or_default()
            .contains("still reads #12 as a draft"),
        "{v}"
    );
    assert_eq!(bodies.lock().unwrap().len(), 1, "the mutation was sent");
}

#[tokio::test]
async fn run_status_asks_for_a_branch_and_refuses_a_bad_repo_before_any_request() {
    state::isolate();
    let (mut t, seen) = rig().await;
    t.policy = allowing(r#""run_status""#);
    let c = ctx("work");
    let (err, v) = call(
        &t,
        &c,
        "run_status",
        json!({ "repo": "owner/name", "branch": "main" }),
    )
    .await;
    assert!(!err, "{v}");
    assert_eq!(v["runs"][0]["conclusion"], "success");
    assert_eq!(
        seen.lock().unwrap()[0].1,
        "/repos/owner/name/actions/runs?branch=main&per_page=10"
    );

    let (err, _) = call(
        &t,
        &c,
        "run_status",
        json!({ "repo": "../etc", "branch": "main" }),
    )
    .await;
    assert!(err);
    let (err, _) = call(&t, &c, "run_status", json!({ "repo": "owner/name" })).await;
    assert!(err);
    assert_eq!(seen.lock().unwrap().len(), 1, "the bad calls sent nothing");
}

#[tokio::test]
async fn pr_threads_reads_threads_and_conversation_in_one_call() {
    state::isolate();
    let (mut t, seen, bodies) = rig_with_bodies().await;
    // Shipped policy: a read runs unattended.
    let c = ctx("work");
    let args = json!({ "repo": "owner/name", "number": 7 });
    let (err, v) = call(&t, &c, "pr_threads", args.clone()).await;
    assert!(!err, "{v}");
    assert_eq!(v["threads"].as_array().unwrap().len(), 2);
    assert_eq!(v["threads"][0]["id"], "PRRT_open");
    assert_eq!(v["threads"][0]["comments"][0]["body"], "rename this");
    assert_eq!(
        v["threads"][1]["line"], 3,
        "an outdated thread keeps its original line"
    );
    assert_eq!(v["comments"][0]["body"], "general note");
    assert_eq!(v["more_threads"], false);
    let sent = bodies.lock().unwrap()[0].clone();
    assert_eq!(sent["variables"]["owner"], "owner");
    assert_eq!(sent["variables"]["name"], "name");
    assert_eq!(sent["variables"]["number"], 7);
    assert!(seen
        .lock()
        .unwrap()
        .iter()
        .all(|(_, _, auth, _)| auth == "Bearer gh-secret"));

    let (err, v) = call(
        &t,
        &c,
        "pr_threads",
        json!({ "repo": "owner/name", "number": 7, "unresolved_only": true }),
    )
    .await;
    assert!(!err, "{v}");
    assert_eq!(v["threads"].as_array().unwrap().len(), 1);

    // GraphQL answers 200 with errors; that is a failure, not an empty review.
    t.policy = allowing(r#""pr_threads""#);
    let (err, v) = call(
        &t,
        &c,
        "pr_threads",
        json!({ "repo": "owner/name", "number": 404 }),
    )
    .await;
    assert!(err, "{v}");
    assert!(v.to_string().contains("Could not resolve"), "{v}");
}

#[tokio::test]
async fn a_thread_reply_waits_on_the_operator_then_replies_and_resolves() {
    state::isolate();
    let (mut t, _, bodies) = rig_with_bodies().await;
    let c = ctx("work");
    let args = json!({ "repo": "owner/name", "number": 7, "thread_id": "PRRT_open",
                       "body": "renamed", "resolve": true });
    let (err, v) = call(&t, &c, "pr_reply", args.clone()).await;
    assert!(err);
    assert!(v.as_str().unwrap_or_default().contains("approval"), "{v}");
    assert!(bodies.lock().unwrap().is_empty(), "nothing reached GitHub");

    t.policy = allowing(r#""pr_reply""#);
    let (err, v) = call(&t, &c, "pr_reply", args).await;
    assert!(!err, "{v}");
    assert_eq!(v["url"], "https://github.test/owner/name/pull/7#r9");
    assert_eq!(v["resolved"], true);
    let sent = bodies.lock().unwrap().clone();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0]["variables"]["thread"], "PRRT_open");
    assert_eq!(sent[0]["variables"]["body"], "renamed");
    assert!(sent[1]["query"]
        .as_str()
        .unwrap()
        .contains("resolveReviewThread"));

    // Neither a reply nor a resolve is nothing to do; a bad id never leaves.
    let (err, _) = call(
        &t,
        &c,
        "pr_reply",
        json!({ "repo": "owner/name", "number": 7, "thread_id": "PRRT_open" }),
    )
    .await;
    assert!(err);
    let (err, _) = call(
        &t,
        &c,
        "pr_reply",
        json!({ "repo": "owner/name", "number": 7, "thread_id": "PRRT open; drop", "body": "b" }),
    )
    .await;
    assert!(err);
    assert_eq!(bodies.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn pr_for_branch_finds_the_open_pull_request() {
    state::isolate();
    let (t, seen) = rig().await;
    let (err, v) = call(
        &t,
        &ctx("work"),
        "pr_for_branch",
        json!({ "repo": "owner/name", "branch": "feat/thing" }),
    )
    .await;
    assert!(!err, "{v}");
    assert_eq!(v["open"][0]["number"], 7);
    assert_eq!(
        seen.lock().unwrap()[0].1,
        "/repos/owner/name/pulls?state=open&head=owner:feat/thing&per_page=10"
    );
}

/// GitHub's CI reads as GitLab's do: a run's jobs say which step failed, and
/// the end of a failed job's log is one read away, bounded the same way.
#[tokio::test]
async fn a_failed_run_is_read_down_to_the_end_of_its_log() {
    state::isolate();
    let (t, _) = rig().await;
    let c = ctx("work");
    let (err, v) = call(
        &t,
        &c,
        "run_status",
        json!({ "repo": "owner/name", "run_id": 900 }),
    )
    .await;
    assert!(!err, "{v}");
    assert_eq!(v["run"]["conclusion"], "failure");
    assert_eq!(v["jobs"][0]["id"], 31);
    assert_eq!(v["jobs"][0]["failed_steps"], json!(["cargo test"]));
    assert_eq!(v["more_jobs"], false);

    let (err, v) = call(
        &t,
        &c,
        "run_logs",
        json!({ "repo": "owner/name", "job_id": 31, "kib": 1 }),
    )
    .await;
    assert!(!err, "{v}");
    assert_eq!(v["truncated"], true);
    let log = v["log"].as_str().unwrap();
    assert!(log.len() <= 1024, "{}", log.len());
    assert!(log.ends_with("error: test failed, to rerun pass `-p node`\n"));
    assert!(
        STORAGE_AUTH.lock().unwrap().iter().all(|a| a.is_empty()),
        "the token is not handed to the log storage GitHub redirects to"
    );
}

/// Rerunning a run's failed jobs is asked, and then runs once.
#[tokio::test]
async fn rerunning_failed_jobs_waits_on_the_operator() {
    state::isolate();
    let (mut t, seen) = rig().await;
    let c = ctx("work");
    let args = json!({ "repo": "owner/name", "run_id": 900 });
    let (err, v) = call(&t, &c, "run_rerun", args.clone()).await;
    assert!(err);
    assert!(v.as_str().unwrap_or_default().contains("approval"), "{v}");
    assert!(seen.lock().unwrap().is_empty(), "nothing reached GitHub");

    t.policy = allowing(r#""run_rerun""#);
    let (err, v) = call(&t, &c, "run_rerun", args).await;
    assert!(!err, "{v}");
    assert_eq!(v["run_id"], 900);
    // The attempt it starts is the one after the run's latest.
    assert_eq!(v["attempt"], 2);
    let seen = seen.lock().unwrap().clone();
    let calls: Vec<(&str, &str)> = seen
        .iter()
        .map(|(method, path, ..)| (method.as_str(), path.as_str()))
        .collect();
    assert_eq!(
        calls,
        [
            ("GET", "/repos/owner/name/actions/runs/900"),
            (
                "POST",
                "/repos/owner/name/actions/runs/900/rerun-failed-jobs"
            )
        ]
    );
}

fn dispatching(workflows: &[&str]) -> Arc<tracon::config::Config> {
    Arc::new(tracon::config::Config {
        github: tracon::config::Github {
            dispatch: [(
                "owner/name".to_string(),
                workflows.iter().map(|w| w.to_string()).collect(),
            )]
            .into(),
        },
        ..Default::default()
    })
}

/// Starting a workflow is asked; a workflow the node's configuration does
/// not list is refused before anyone is asked, and a tag is refused before
/// anything is sent.
#[tokio::test]
async fn a_workflow_is_dispatched_only_when_listed_and_asked() {
    state::isolate();
    let (mut t, seen, bodies) = rig_with_bodies().await;
    let c = ctx("work");
    let args = json!({ "repo": "owner/name", "workflow": "renovate.yaml", "ref": "main",
                       "inputs": { "LOG_LEVEL": "debug" } });

    // Nothing listed: refused outright, not asked.
    let (err, v) = call(&t, &c, "run_dispatch", args.clone()).await;
    assert!(err);
    let said = v.as_str().unwrap_or_default();
    assert!(said.contains("[github] dispatch"), "{v}");
    assert!(!said.contains("approval"), "{v}");

    t.cfg = dispatching(&["renovate.yaml", "quiet.yml"]);
    let (err, v) = call(&t, &c, "run_dispatch", args.clone()).await;
    assert!(err);
    assert!(v.as_str().unwrap_or_default().contains("approval"), "{v}");
    assert!(seen.lock().unwrap().is_empty(), "nothing reached GitHub");

    t.policy = allowing(r#""run_dispatch""#);
    let mut release = args.clone();
    release["workflow"] = json!("release.yml");
    let (err, v) = call(&t, &c, "run_dispatch", release).await;
    assert!(
        err && v.as_str().unwrap_or_default().contains("[github] dispatch"),
        "{v}"
    );
    let mut production = args.clone();
    production["inputs"] = json!({ "TARGET": "production" });
    let (err, v) = call(&t, &c, "run_dispatch", production).await;
    assert!(
        err && v.as_str().unwrap_or_default().contains("names production"),
        "{v}"
    );
    assert!(seen.lock().unwrap().is_empty(), "nothing reached GitHub");

    let mut tag = args.clone();
    tag["ref"] = json!("v1");
    let (err, v) = call(&t, &c, "run_dispatch", tag).await;
    assert!(
        err && v.as_str().unwrap_or_default().contains("is a tag"),
        "{v}"
    );
    assert!(
        !seen.lock().unwrap().iter().any(|(m, ..)| m == "POST"),
        "a tag is not dispatched"
    );
    seen.lock().unwrap().clear();

    let (err, v) = call(&t, &c, "run_dispatch", args).await;
    assert!(!err, "{v}");
    assert_eq!(v["run_id"], 950);
    assert_eq!(v["url"], "https://github.test/owner/name/actions/runs/950");
    let calls: Vec<(String, String)> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|(m, p, ..)| (m.clone(), p.clone()))
        .collect();
    assert_eq!(
        calls.last().unwrap(),
        &(
            "POST".to_string(),
            "/repos/owner/name/actions/workflows/renovate.yaml/dispatches".to_string()
        )
    );
    let sent = bodies.lock().unwrap().last().cloned().unwrap();
    assert_eq!(
        sent,
        json!({ "ref": "main", "inputs": { "LOG_LEVEL": "debug" }, "return_run_details": true })
    );
}

/// A GitHub that answers 204 without the run leaves the node to find it.
#[tokio::test]
async fn a_dispatch_github_does_not_name_is_found_in_the_list() {
    state::isolate();
    let (mut t, _) = rig().await;
    t.cfg = dispatching(&["quiet.yml"]);
    t.policy = allowing(r#""run_dispatch""#);
    let (err, v) = call(
        &t,
        &ctx("work"),
        "run_dispatch",
        json!({ "repo": "owner/name", "workflow": "quiet.yml", "ref": "main" }),
    )
    .await;
    assert!(!err, "{v}");
    assert_eq!(v["run_id"], 961);
    assert_eq!(v["url"], "https://github.test/owner/name/actions/runs/961");
}

/// A session on `owner/name` (resolved from its checkout's `origin` into the
/// project row, as launch does), its continuation on a retained workspace,
/// and the tools wired to the node's store so a call can find them.
async fn session_rig(policy: &str) -> (Arc<Tools>, Seen) {
    use tracon::store::{ProjectRow, Store};
    let (mut tools, seen) = rig().await;
    tools.policy = allowing(policy);
    let tools = Arc::new(tools);
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.ensure_peer_node("n1").unwrap();
    store.channel_put("work", &[], "{}").unwrap();
    for (project, remote) in [
        ("p-checkout", Some("github.com/owner/name")),
        ("p-workspace", None),
    ] {
        store
            .project_put(&ProjectRow {
                id: project.into(),
                channel: "work".into(),
                name: project.into(),
                remote_url: remote.map(String::from),
                created_ms: 1,
            })
            .unwrap();
    }
    let mut first = support::rows::session_row("s1", "n1", "work");
    first.project_id = Some("p-checkout".into());
    first.state = "closed".into();
    store.insert_session(&first).unwrap();
    // A continuation runs on the retained workspace, whose project has no
    // remote: its repository is its lineage's.
    let mut next = support::rows::session_row("s2", "n1", "work");
    next.project_id = Some("p-workspace".into());
    next.repo_path = "workspace://s1".into();
    next.continued_from = Some("s1".into());
    store.insert_session(&next).unwrap();
    // And one on a local-only repository, which has nothing to default to.
    let mut local = support::rows::session_row("s3", "n1", "work");
    local.project_id = Some("p-workspace".into());
    store.insert_session(&local).unwrap();
    let cfg = tools.cfg.clone();
    let manager = tracon::session::Manager::new(
        store.clone(),
        tracon::stream::Bus::new(),
        cfg,
        "n1".into(),
        tools.clone(),
        Default::default(),
        Arc::new(tracon::runner::local::LocalBackend),
    );
    let _ = tools
        .session
        .set(tracon::mcp::SessionAccess { store, manager });
    (tools, seen)
}

/// A session's workspace has no remote, so a forge tool it calls without
/// `repo` means the session's own repository; nothing else is guessed.
#[tokio::test]
async fn a_session_call_without_repo_reaches_its_own_repository() {
    state::isolate();
    let (t, seen) = session_rig(r#""run_status""#).await;
    let session = CallContext::session("s2", "work", "n1");
    let (err, v) = call(&t, &session, "run_status", json!({ "branch": "main" })).await;
    assert!(!err, "{v}");
    assert_eq!(v["runs"][0]["conclusion"], "success");
    assert_eq!(
        seen.lock().unwrap().last().unwrap().1,
        "/repos/owner/name/actions/runs?branch=main&per_page=10"
    );

    // An explicit repository wins over the default.
    let (_, _) = call(
        &t,
        &session,
        "run_status",
        json!({ "repo": "other/thing", "branch": "main" }),
    )
    .await;
    assert_eq!(
        seen.lock().unwrap().last().unwrap().1,
        "/repos/other/thing/actions/runs?branch=main&per_page=10"
    );

    // A session on a local-only repository, and an external harness, still
    // have to name one, and are refused before anything is sent.
    let sent = seen.lock().unwrap().len();
    for caller in [
        CallContext::session("s3", "work", "n1"),
        CallContext::external(None, "work", "n1"),
    ] {
        let (err, v) = call(&t, &caller, "run_status", json!({ "branch": "main" })).await;
        assert!(err, "{v}");
        assert!(v.to_string().contains("repo is required"), "{v}");
    }
    assert_eq!(
        seen.lock().unwrap().len(),
        sent,
        "the refused calls sent nothing"
    );
}

/// What each caller is offered says the same: `repo` is optional only to a
/// session that has a repository for it to default to.
#[tokio::test]
async fn repo_is_optional_only_where_it_defaults() {
    state::isolate();
    let (t, _) = session_rig(r#""run_status""#).await;
    let required = |ctx: &CallContext, tool: &str| -> Vec<String> {
        t.list_offered(ctx)
            .into_iter()
            .find(|d| d["name"] == tool)
            .unwrap()["inputSchema"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    };
    let session = CallContext::session("s2", "work", "n1");
    assert_eq!(required(&session, "run_status"), Vec::<String>::new());
    assert_eq!(
        required(&session, "pr_merge"),
        ["number", "head_sha", "operation_id"]
    );
    for other in [
        CallContext::session("s3", "work", "n1"),
        CallContext::external(None, "work", "n1"),
    ] {
        assert_eq!(required(&other, "run_status"), ["repo"]);
    }
    let described = t
        .list_offered(&session)
        .into_iter()
        .find(|d| d["name"] == "run_status")
        .unwrap();
    assert!(
        described["inputSchema"]["properties"]["repo"]["description"]
            .as_str()
            .unwrap()
            .contains("defaults to that repository")
    );
}

/// Defaulting a session's repository edits `required` lists; it must not add
/// one where a tool has none. A `"required": null` is not a JSON Schema, and
/// Claude Code drops every tool of a server that offers one.
#[tokio::test]
async fn offered_schemas_hold_no_null() {
    state::isolate();
    let (t, _) = session_rig(r#""run_status""#).await;
    fn nulls(v: &Value, at: &str, found: &mut Vec<String>) {
        match v {
            Value::Null => found.push(at.to_string()),
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    nulls(item, &format!("{at}[{i}]"), found);
                }
            }
            Value::Object(fields) => {
                for (k, item) in fields {
                    nulls(item, &format!("{at}.{k}"), found);
                }
            }
            _ => {}
        }
    }
    let external = CallContext::external(None, "work", "n1");
    assert!(
        t.list_offered(&external)
            .iter()
            .any(|d| d["inputSchema"].get("required").is_none()),
        "a tool with nothing required is offered, or this proves nothing"
    );
    for caller in [
        CallContext::session("s2", "work", "n1"),
        CallContext::session("s3", "work", "n1"),
        external,
    ] {
        let offered = t.list_offered(&caller);
        let mut found = Vec::new();
        for d in &offered {
            nulls(d, d["name"].as_str().unwrap(), &mut found);
            if let Some(required) = d["inputSchema"].get("required") {
                assert!(required.is_array(), "{}: {required}", d["name"]);
            }
        }
        assert!(found.is_empty(), "nulls offered: {found:?}");
    }
}

/// A session in each phase, on the rig's channel: `plan`, `exec` and
/// `review`, each in that phase.
async fn phase_rig() -> Arc<Tools> {
    let (t, _) = session_rig(r#""run_status""#).await;
    let store = &t.session.get().unwrap().store;
    for (id, phase) in [("plan", "plan"), ("exec", "execute"), ("review", "review")] {
        let mut row = support::rows::session_row(id, "n1", "work");
        row.phase = phase.into();
        store.insert_session(&row).unwrap();
    }
    t
}

/// The names `tools/list` answers the caller with, in order.
async fn listed(t: &Tools, c: &CallContext) -> Vec<String> {
    let res = t
        .handle(c, &json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
        .await
        .unwrap();
    res["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap().to_string())
        .collect()
}

/// Each phase is offered its own list and nothing else, narrowed again to
/// what the channel has credentials for: a plan session is not handed the
/// tools that submit, merge or set a repository up.
#[tokio::test]
async fn each_phase_lists_exactly_its_tools_of_the_channels() {
    state::isolate();
    let t = phase_rig().await;
    let channel: Vec<String> = t
        .list("work", "n1")
        .iter()
        .map(|d| d["name"].as_str().unwrap().to_string())
        .collect();
    let narrowed = |offers: &dyn Fn(&str) -> bool| -> Vec<String> {
        channel.iter().filter(|n| offers(n)).cloned().collect()
    };
    let plan = listed(&t, &CallContext::session("plan", "work", "n1")).await;
    assert_eq!(plan, narrowed(&|n| Tools::PLAN_TOOLS.contains(&n)));
    // The channel has GitHub and no GitLab or Jira: GitHub's reading tools
    // are there, its writes are not, and nothing of the others is.
    for offered in [
        "doc_write",
        "ask_operator",
        "work_discover",
        "pr_status",
        "run_wait",
    ] {
        assert!(
            plan.iter().any(|n| n == offered),
            "{offered} not in {plan:?}"
        );
    }
    for withheld in [
        "work_close",
        "submit_review",
        "pr_merge",
        "pr_comment",
        "github_issue_comment",
        "pr_ready",
        "run_dispatch",
        "repo_setup_draft",
        "request_egress",
        "show_work",
        "brief_note",
        "criteria_link",
        "review_verdict",
        "review_finding",
        "mr_status",
        "issue",
    ] {
        assert!(
            !plan.iter().any(|n| n == withheld),
            "{withheld} in {plan:?}"
        );
    }

    let exec = listed(&t, &CallContext::session("exec", "work", "n1")).await;
    assert_eq!(exec, narrowed(&|n| !Tools::NOT_EXECUTE.contains(&n)));
    // A verdict and findings are a review session's account of someone
    // else's change; everything else is the execute session's to use.
    for withheld in ["review_verdict", "review_finding"] {
        assert!(
            !exec.iter().any(|n| n == withheld),
            "{withheld} in {exec:?}"
        );
    }
    assert_eq!(exec.len(), channel.len() - 2, "{exec:?}");

    let review = listed(&t, &CallContext::session("review", "work", "n1")).await;
    assert_eq!(review, narrowed(&|n| Tools::REVIEW_TOOLS.contains(&n)));

    let external = listed(&t, &CallContext::external(None, "work", "n1")).await;
    assert_eq!(external, narrowed(&|n| !Tools::NOT_EXTERNAL.contains(&n)));
    for withheld in [
        "review_verdict",
        "review_finding",
        "brief_read",
        "brief_note",
        "criteria_read",
        "criteria_link",
    ] {
        assert!(
            !external.iter().any(|n| n == withheld),
            "{withheld} in {external:?}"
        );
    }
    assert!(
        external.iter().any(|n| n == "submit_review"),
        "{external:?}"
    );
}

/// A tool a phase is not offered is refused when called, not only left off
/// the list, and the refusal names the phase.
#[tokio::test]
async fn a_plan_session_calling_work_close_or_submit_review_is_refused() {
    state::isolate();
    let t = phase_rig().await;
    let plan = CallContext::session("plan", "work", "n1");
    for (tool, args) in [
        ("work_close", json!({ "summary": "done" })),
        ("submit_review", json!({ "title": "t", "body": "b" })),
    ] {
        let (err, v) = call(&t, &plan, tool, args).await;
        assert!(err, "{v}");
        assert!(
            v.to_string()
                .contains(&format!("{tool} is not offered in a plan session")),
            "{v}"
        );
    }
}
