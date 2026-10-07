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
    if (method.as_str(), path) == ("POST", "/graphql") {
        let sent: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        bodies.lock().unwrap().push(sent.clone());
        let query = sent["query"].as_str().unwrap_or_default();
        return ok(if query.contains("addPullRequestReviewThreadReply") {
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
    for n in ["pr_status", "pr_comment", "run_status"] {
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
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        (seen[0].0.as_str(), seen[0].1.as_str()),
        (
            "POST",
            "/repos/owner/name/actions/runs/900/rerun-failed-jobs"
        )
    );
}
